//! Minimal WHATWG Fetch bindings (`fetch`/`Headers`/`Request`/`Response`) for the `vm-js` Window realm.
//!
//! This is an MVP binding layer:
//! - It is **not** a complete Fetch implementation (no streaming bodies, no full `RequestInit`,
//!   etc).
//! - It is intended to expose enough surface area for early deterministic tests and real-world
//!   scripts that expect `fetch()` to exist.
//!
//! The core Fetch algorithms and spec-shaped data structures live in `crate::resource::web_fetch`.
//! This module is the missing JavaScript-facing wrapper layer for the `WindowRealm` (`vm-js`)
//! embedding.

use crate::js::event_loop::TaskSource;
use crate::js::url_resolve::{resolve_url, UrlResolveError};
use crate::js::window_blob;
use crate::js::window_form_data;
use crate::js::window_realm::{WindowRealmHost, WindowRealmUserData};
use crate::js::window_timers::{event_loop_mut_from_hooks, vm_error_to_event_loop_error, VmJsEventLoopHooks};
use crate::js::window_url;
use crate::resource::web_fetch::{
  execute_web_fetch, Body, Headers as CoreHeaders, HeadersGuard, Request as CoreRequest, Response as CoreResponse,
  RequestCredentials, RequestMode, RequestRedirect, ResponseType, WebFetchExecutionContext, WebFetchError, WebFetchLimits,
};
use crate::resource::{origin_from_url, DocumentOrigin, FetchDestination, ReferrerPolicy, ResourceFetcher};
use http::Method;
use std::char::decode_utf16;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use vm_js::{
  GcObject, Heap, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks,
};

const SLOT_ENV_ID: usize = 0;
const SLOT_HEADERS_PROTO: usize = 1;
const SLOT_RESPONSE_PROTO: usize = 2;

// Hidden per-instance properties.
const ENV_ID_KEY: &str = "__fastrender_fetch_env_id";

const HEADERS_KIND_KEY: &str = "__fastrender_headers_kind";
const HEADERS_OWNER_KEY: &str = "__fastrender_headers_owner";

const REQUEST_ID_KEY: &str = "__fastrender_request_id";
const RESPONSE_ID_KEY: &str = "__fastrender_response_id";

// Internal helper keys for Promise capability construction via `new Promise(executor)`.
const PROMISE_CAP_RESOLVE_KEY: &str = "__fastrender_promise_cap_resolve";
const PROMISE_CAP_REJECT_KEY: &str = "__fastrender_promise_cap_reject";

// Discriminant for how a JS `Headers` wrapper is backed.
const HEADERS_KIND_OWNED: u8 = 0;
const HEADERS_KIND_REQUEST: u8 = 1;
const HEADERS_KIND_RESPONSE: u8 = 2;

// Hidden per-instance properties for `Headers` iterators.
const HEADERS_ITER_ID_KEY: &str = "__fastrender_headers_iter_id";
const HEADERS_ITER_KIND_KEY: &str = "__fastrender_headers_iter_kind";
const HEADERS_ITER_DONE_KEY: &str = "__fastrender_headers_iter_done";

const HEADERS_ITER_KIND_ENTRIES: u8 = 0;
const HEADERS_ITER_KIND_KEYS: u8 = 1;
const HEADERS_ITER_KIND_VALUES: u8 = 2;

#[derive(Clone)]
pub struct WindowFetchEnv {
  pub fetcher: Arc<dyn ResourceFetcher>,
  pub document_url: Option<String>,
  pub document_origin: Option<DocumentOrigin>,
  pub referrer_policy: ReferrerPolicy,
  pub limits: WebFetchLimits,
}

impl WindowFetchEnv {
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
  env: WindowFetchEnv,
  promise_executor_call: NativeFunctionId,
  next_id: u64,
  multipart_boundary_counter: u64,
  owned_headers: HashMap<u64, CoreHeaders>,
  requests: HashMap<u64, CoreRequest>,
  responses: HashMap<u64, CoreResponse>,
  headers_iterators: HashMap<u64, HeadersIteratorState>,
}

struct HeadersIteratorState {
  pairs: Vec<(String, String)>,
  index: usize,
}

impl EnvState {
  fn new(env: WindowFetchEnv, promise_executor_call: NativeFunctionId) -> Self {
    Self {
      env,
      promise_executor_call,
      next_id: 1,
      multipart_boundary_counter: 1,
      owned_headers: HashMap::new(),
      requests: HashMap::new(),
      responses: HashMap::new(),
      headers_iterators: HashMap::new(),
    }
  }

  fn alloc_id(&mut self) -> u64 {
    let id = self.next_id;
    self.next_id = self.next_id.saturating_add(1);
    id
  }
}

static NEXT_ENV_ID: AtomicU64 = AtomicU64::new(1);
static ENVS: OnceLock<Mutex<HashMap<u64, EnvState>>> = OnceLock::new();

fn envs() -> &'static Mutex<HashMap<u64, EnvState>> {
  ENVS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn unregister_window_fetch_env(env_id: u64) {
  let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  lock.remove(&env_id);
}

/// RAII guard returned by [`install_window_fetch_bindings_with_guard`].
///
/// Dropping this guard unregisters the backing Rust state for `fetch`/`Headers`/`Request`/`Response`
/// installed into a given `vm-js` realm.
///
/// This mirrors the `TimeBindings` pattern in `src/js/time.rs`: callers should keep the returned
/// value alive for at least as long as the JS realm is reachable.
#[derive(Debug)]
#[must_use = "fetch bindings are only valid while the returned WindowFetchBindings is kept alive"]
pub struct WindowFetchBindings {
  env_id: u64,
  active: bool,
}

impl WindowFetchBindings {
  fn new(env_id: u64) -> Self {
    Self {
      env_id,
      active: true,
    }
  }

  /// Returns the internal env id used to associate JS wrapper objects with their Rust state.
  pub fn env_id(&self) -> u64 {
    self.env_id
  }

  /// Disable automatic cleanup and return the env id.
  ///
  /// This is used by the legacy `install_window_fetch_bindings` API, which returns the env id and
  /// requires callers to manually invoke [`unregister_window_fetch_env`].
  fn disarm(mut self) -> u64 {
    self.active = false;
    self.env_id
  }
}

impl Drop for WindowFetchBindings {
  fn drop(&mut self) {
    if self.active {
      unregister_window_fetch_env(self.env_id);
    }
  }
}

fn with_env_state<R>(env_id: u64, f: impl FnOnce(&EnvState) -> Result<R, VmError>) -> Result<R, VmError> {
  let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get(&env_id)
    .ok_or(VmError::Unimplemented("fetch env id not registered"))?;
  f(state)
}

fn with_env_state_mut<R>(
  env_id: u64,
  f: impl FnOnce(&mut EnvState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get_mut(&env_id)
    .ok_or(VmError::Unimplemented("fetch env id not registered"))?;
  f(state)
}

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

fn alloc_symbol_key(scope: &mut Scope<'_>, description: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(description)?;
  scope.push_root(Value::String(s))?;
  let sym = scope.heap_mut().symbol_for(s)?;
  Ok(PropertyKey::from_symbol(sym))
}

fn current_document_base_url(vm: &mut Vm, env_id: u64) -> Result<Option<String>, VmError> {
  if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
    return Ok(data.base_url.clone());
  }
  with_env_state(env_id, |state| Ok(state.env.document_url.clone()))
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
  // Root `obj` and `value` while allocating the property key: `alloc_key` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

const FETCH_URL_TOO_LONG_ERROR: &str = "fetch URL exceeds maximum length";
const FETCH_METHOD_TOO_LONG_ERROR: &str = "fetch method exceeds maximum length";
const FETCH_HEADER_NAME_TOO_LONG_ERROR: &str = "fetch header name exceeds maximum length";
const FETCH_HEADER_VALUE_TOO_LONG_ERROR: &str = "fetch header value exceeds maximum length";
const FETCH_BODY_TOO_LONG_ERROR: &str = "fetch body exceeds maximum length";
const FETCH_CREDENTIALS_TOO_LONG_ERROR: &str = "Request.credentials exceeds maximum length";
const FETCH_MODE_TOO_LONG_ERROR: &str = "Request.mode exceeds maximum length";
const FETCH_REDIRECT_TOO_LONG_ERROR: &str = "Request.redirect exceeds maximum length";
const FETCH_REFERRER_TOO_LONG_ERROR: &str = "Request.referrer exceeds maximum length";
const FETCH_REFERRER_POLICY_TOO_LONG_ERROR: &str = "Request.referrerPolicy exceeds maximum length";
const FETCH_STATUS_TEXT_TOO_LONG_ERROR: &str = "Response statusText exceeds maximum length";

fn js_string_to_rust_string_limited(
  heap: &Heap,
  handle: vm_js::GcString,
  max_bytes: usize,
  error: &'static str,
) -> Result<String, VmError> {
  let js = heap.get_string(handle)?;

  let code_units_len = js.len_code_units();
  // UTF-8 output bytes are always >= UTF-16 code unit length (and can grow by up to 3 bytes per
  // code unit when decoding lone surrogates as U+FFFD). Reject overly large strings up-front to
  // prevent unbounded host allocations.
  if code_units_len > max_bytes {
    return Err(VmError::TypeError(error));
  }

  // Decode manually so we can enforce the byte limit without relying on the potentially-large
  // allocation performed by `String::from_utf16_lossy`.
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

fn number_to_u64(value: Value) -> Result<u64, VmError> {
  let Value::Number(n) = value else {
    return Err(VmError::TypeError("expected number"));
  };
  if !n.is_finite() || n < 0.0 || n > u64::MAX as f64 {
    return Err(VmError::TypeError("number out of range"));
  }
  Ok(n as u64)
}

fn number_to_u16_wrapping(n: f64) -> u16 {
  // WebIDL integer conversions for `unsigned short` use `ToUint16` (wrap modulo 2^16).
  if !n.is_finite() {
    return 0;
  }
  let n = n.trunc();
  (n.rem_euclid(65536.0)) as u16
}

fn is_reason_phrase_byte_string(s: &str) -> bool {
  // Fetch `ResponseInit.statusText` is a `ByteString` and must match the HTTP
  // `reason-phrase = *( HTAB / SP / VCHAR / obs-text )` production (RFC 9110).
  //
  // Allowed bytes:
  // - HTAB (0x09)
  // - SP (0x20)
  // - VCHAR (0x21..=0x7E)
  // - obs-text (0x80..=0xFF)
  //
  // Reject any non-Latin-1 scalar values (enforces `ByteString`) and ASCII control bytes other
  // than HTAB.
  s.chars().all(|ch| {
    let b = ch as u32;
    matches!(b, 0x09 | 0x20..=0x7E | 0x80..=0xFF)
  })
}

fn request_mode_to_string(mode: RequestMode) -> &'static str {
  match mode {
    RequestMode::Navigate => "navigate",
    RequestMode::SameOrigin => "same-origin",
    RequestMode::NoCors => "no-cors",
    RequestMode::Cors => "cors",
  }
}

fn request_credentials_to_string(credentials: RequestCredentials) -> &'static str {
  match credentials {
    RequestCredentials::Omit => "omit",
    RequestCredentials::SameOrigin => "same-origin",
    RequestCredentials::Include => "include",
  }
}

fn request_redirect_to_string(redirect: RequestRedirect) -> &'static str {
  match redirect {
    RequestRedirect::Follow => "follow",
    RequestRedirect::Error => "error",
    RequestRedirect::Manual => "manual",
  }
}

fn response_type_to_string(r#type: ResponseType) -> &'static str {
  match r#type {
    ResponseType::Basic => "basic",
    ResponseType::Cors => "cors",
    ResponseType::Default => "default",
    ResponseType::Error => "error",
    ResponseType::Opaque => "opaque",
    ResponseType::OpaqueRedirect => "opaqueredirect",
  }
}

fn normalize_and_validate_method(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  method: &str,
) -> Result<String, VmError> {
  if Method::from_bytes(method.as_bytes()).is_err() {
    return Err(throw_type_error(
      vm,
      scope,
      host,
      host_hooks,
      "Request.method is not a valid HTTP method token",
    ));
  }

  if method.eq_ignore_ascii_case("CONNECT")
    || method.eq_ignore_ascii_case("TRACE")
    || method.eq_ignore_ascii_case("TRACK")
  {
    return Err(throw_type_error(
      vm,
      scope,
      host,
      host_hooks,
      "Request.method is forbidden",
    ));
  }

  let normalized = if method.eq_ignore_ascii_case("DELETE") {
    "DELETE"
  } else if method.eq_ignore_ascii_case("GET") {
    "GET"
  } else if method.eq_ignore_ascii_case("HEAD") {
    "HEAD"
  } else if method.eq_ignore_ascii_case("OPTIONS") {
    "OPTIONS"
  } else if method.eq_ignore_ascii_case("POST") {
    "POST"
  } else if method.eq_ignore_ascii_case("PUT") {
    "PUT"
  } else {
    method
  };

  Ok(normalized.to_string())
}

fn create_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  ctor: GcObject,
  message: &str,
) -> Result<Value, VmError> {
  let msg = scope.alloc_string(message)?;
  scope.push_root(Value::String(msg))?;
  vm.construct_with_host_and_hooks(host, scope, hooks, Value::Object(ctor), &[Value::String(msg)], Value::Object(ctor))
}

fn create_type_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  message: &str,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "TypeError requires intrinsics (create a Realm first)",
  ))?;
  create_error(vm, scope, host, hooks, intr.type_error(), message)
}

fn create_range_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  message: &str,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "RangeError requires intrinsics (create a Realm first)",
  ))?;
  create_error(vm, scope, host, hooks, intr.range_error(), message)
}

fn create_syntax_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  message: &str,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "SyntaxError requires intrinsics (create a Realm first)",
  ))?;
  create_error(vm, scope, host, hooks, intr.syntax_error(), message)
}

fn throw_type_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  message: &str,
) -> VmError {
  match create_type_error(vm, scope, host, hooks, message) {
    Ok(err) => VmError::Throw(err),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn throw_range_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  message: &str,
) -> VmError {
  match create_range_error(vm, scope, host, hooks, message) {
    Ok(err) => VmError::Throw(err),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn throw_syntax_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  message: &str,
) -> VmError {
  match create_syntax_error(vm, scope, host, hooks, message) {
    Ok(err) => VmError::Throw(err),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn map_web_fetch_error_to_throw(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  err: WebFetchError,
) -> VmError {
  match err {
    WebFetchError::BodyInvalidJson(e) => throw_syntax_error(vm, scope, host, hooks, &e.to_string()),
    other => throw_type_error(vm, scope, host, hooks, &other.to_string()),
  }
}
fn env_id_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<u64, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let value = slots.get(SLOT_ENV_ID).copied().unwrap_or(Value::Undefined);
  number_to_u64(value)
}

fn headers_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(SLOT_HEADERS_PROTO).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "fetch binding missing Headers.prototype native slot",
    )),
  }
}

fn response_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(SLOT_RESPONSE_PROTO).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "fetch binding missing Response.prototype native slot",
    )),
  }
}

fn request_info_from_value(scope: &mut Scope<'_>, value: Value) -> Option<(u64, u64)> {
  let Value::Object(obj) = value else {
    return None;
  };
  let env_id = get_data_prop(scope, obj, ENV_ID_KEY).ok()?;
  let request_id = get_data_prop(scope, obj, REQUEST_ID_KEY).ok()?;
  let env_id = number_to_u64(env_id).ok()?;
  let request_id = number_to_u64(request_id).ok()?;
  Some((env_id, request_id))
}

fn request_info_from_this(scope: &mut Scope<'_>, this: Value) -> Result<(u64, u64), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };

  let env_val = get_data_prop(scope, obj, ENV_ID_KEY)?;
  let request_val = get_data_prop(scope, obj, REQUEST_ID_KEY)?;
  if !matches!(env_val, Value::Number(_)) || !matches!(request_val, Value::Number(_)) {
    return Err(VmError::TypeError("Request: illegal invocation"));
  }

  let env_id =
    number_to_u64(env_val).map_err(|_| VmError::TypeError("Request: illegal invocation"))?;
  let request_id =
    number_to_u64(request_val).map_err(|_| VmError::TypeError("Request: illegal invocation"))?;
  Ok((env_id, request_id))
}

struct JsPromiseCapability {
  promise: Value,
  resolve: Value,
  reject: Value,
}

fn promise_capability_executor_call(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let capture = match slots.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => obj,
    _ => return Err(VmError::InvariantViolation("Promise executor missing capture slot")),
  };

  let resolve = args.get(0).copied().unwrap_or(Value::Undefined);
  let reject = args.get(1).copied().unwrap_or(Value::Undefined);

  set_data_prop(scope, capture, PROMISE_CAP_RESOLVE_KEY, resolve, false)?;
  set_data_prop(scope, capture, PROMISE_CAP_REJECT_KEY, reject, false)?;

  Ok(Value::Undefined)
}

fn new_promise_capability_for_env(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  env_id: u64,
) -> Result<JsPromiseCapability, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "Promise capability requires intrinsics (create a Realm first)",
  ))?;

  let executor_call = with_env_state(env_id, |state| Ok(state.promise_executor_call))?;

  let capture = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(capture))?;

  let executor_name = scope.alloc_string("Promise capability executor")?;
  scope.push_root(Value::String(executor_name))?;
  let executor = scope.alloc_native_function_with_slots(
    executor_call,
    None,
    executor_name,
    2,
    &[Value::Object(capture)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(executor, Some(intr.function_prototype()))?;
  scope.push_root(Value::Object(executor))?;

  let promise_ctor = intr.promise();
  let promise = vm.construct_with_host_and_hooks(
    host,
    scope,
    hooks,
    Value::Object(promise_ctor),
    &[Value::Object(executor)],
    Value::Object(promise_ctor),
  )?;
  let promise = scope.push_root(promise)?;
  if !matches!(promise, Value::Object(_)) {
    return Err(VmError::InvariantViolation(
      "Promise constructor returned non-object",
    ));
  }

  let resolve = get_data_prop(scope, capture, PROMISE_CAP_RESOLVE_KEY)?;
  let reject = get_data_prop(scope, capture, PROMISE_CAP_REJECT_KEY)?;
  if !scope.heap().is_callable(resolve)? || !scope.heap().is_callable(reject)? {
    return Err(VmError::InvariantViolation(
      "Promise executor did not capture resolve/reject",
    ));
  }

  Ok(JsPromiseCapability {
    promise,
    resolve,
    reject,
  })
}

fn headers_info_from_this(scope: &mut Scope<'_>, this: Value) -> Result<(u64, u8, u64), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Headers: illegal invocation"));
  };
  let env_val = get_data_prop(scope, obj, ENV_ID_KEY)?;
  let kind_val = get_data_prop(scope, obj, HEADERS_KIND_KEY)?;
  let owner_val = get_data_prop(scope, obj, HEADERS_OWNER_KEY)?;
  if !matches!(env_val, Value::Number(_))
    || !matches!(kind_val, Value::Number(_))
    || !matches!(owner_val, Value::Number(_))
  {
    return Err(VmError::TypeError("Headers: illegal invocation"));
  }

  let env_id =
    number_to_u64(env_val).map_err(|_| VmError::TypeError("Headers: illegal invocation"))?;
  let kind =
    number_to_u64(kind_val).map_err(|_| VmError::TypeError("Headers: illegal invocation"))?;
  let owner =
    number_to_u64(owner_val).map_err(|_| VmError::TypeError("Headers: illegal invocation"))?;
  let kind_u8: u8 = kind
    .try_into()
    .map_err(|_| VmError::TypeError("Headers: invalid kind"))?;
  Ok((env_id, kind_u8, owner))
}

fn response_info_from_this(scope: &mut Scope<'_>, this: Value) -> Result<(u64, u64), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };

  let env_val = get_data_prop(scope, obj, ENV_ID_KEY)?;
  let response_val = get_data_prop(scope, obj, RESPONSE_ID_KEY)?;
  if !matches!(env_val, Value::Number(_)) || !matches!(response_val, Value::Number(_)) {
    return Err(VmError::TypeError("Response: illegal invocation"));
  }

  let env_id =
    number_to_u64(env_val).map_err(|_| VmError::TypeError("Response: illegal invocation"))?;
  let response_id =
    number_to_u64(response_val).map_err(|_| VmError::TypeError("Response: illegal invocation"))?;
  Ok((env_id, response_id))
}

fn get_headers_mut<'a>(
  state: &'a mut EnvState,
  kind: u8,
  owner: u64,
) -> Result<&'a mut CoreHeaders, VmError> {
  match kind {
    HEADERS_KIND_OWNED => state
      .owned_headers
      .get_mut(&owner)
      .ok_or(VmError::TypeError("Headers: invalid backing object")),
    HEADERS_KIND_REQUEST => state
      .requests
      .get_mut(&owner)
      .map(|r| &mut r.headers)
      .ok_or(VmError::TypeError("Headers: invalid backing request")),
    HEADERS_KIND_RESPONSE => state
      .responses
      .get_mut(&owner)
      .map(|r| &mut r.headers)
      .ok_or(VmError::TypeError("Headers: invalid backing response")),
    _ => Err(VmError::TypeError("Headers: invalid kind")),
  }
}

fn get_headers_ref<'a>(state: &'a EnvState, kind: u8, owner: u64) -> Result<&'a CoreHeaders, VmError> {
  match kind {
    HEADERS_KIND_OWNED => state
      .owned_headers
      .get(&owner)
      .ok_or(VmError::TypeError("Headers: invalid backing object")),
    HEADERS_KIND_REQUEST => state
      .requests
      .get(&owner)
      .map(|r| &r.headers)
      .ok_or(VmError::TypeError("Headers: invalid backing request")),
    HEADERS_KIND_RESPONSE => state
      .responses
      .get(&owner)
      .map(|r| &r.headers)
      .ok_or(VmError::TypeError("Headers: invalid backing response")),
    _ => Err(VmError::TypeError("Headers: invalid kind")),
  }
}

fn fill_headers_from_init(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  env_id: u64,
  headers: &mut CoreHeaders,
  init: Value,
) -> Result<(), VmError> {
  if matches!(init, Value::Undefined | Value::Null) {
    return Ok(());
  }

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let array_proto = intr.array_prototype();

  let Value::Object(obj) = init else {
    return Err(throw_type_error(
      vm,
      scope,
      host,
      hooks,
      "Headers init must be an object",
    ));
  };

  // If this looks like a `Headers` wrapper, clone its pairs.
  let maybe_env = get_data_prop(scope, obj, ENV_ID_KEY).ok();
  if let Some(Value::Number(_)) = maybe_env {
    if let Ok((other_env, kind, owner)) = headers_info_from_this(scope, Value::Object(obj)) {
      let pairs = with_env_state(other_env, |state| {
        let h = get_headers_ref(state, kind, owner)?;
        Ok(h.raw_pairs())
      })?;
      headers
        .fill_from_pairs(pairs)
        .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, hooks, err))?;
      return Ok(());
    }
  }

  // Sequence-of-pairs form: treat Arrays as `sequence<sequence<ByteString>>`.
  if scope.heap().object_prototype(obj)? == Some(array_proto) {
    // Read `.length` as a u32.
    let length_key = alloc_key(scope, "length")?;
    let len_value = vm.get_with_host_and_hooks(host, scope, hooks, obj, length_key)?;
    let len_u64 = number_to_u64(len_value)?;
    let len: usize = len_u64
      .try_into()
      .map_err(|_| throw_type_error(vm, scope, host, hooks, "Headers init array too large"))?;

    let mut sequence: Vec<[String; 2]> = Vec::new();
    sequence
      .try_reserve_exact(len)
      .map_err(|_| VmError::OutOfMemory)?;

    const TICK_EVERY: usize = 256;
    for idx in 0..len {
      if idx % TICK_EVERY == 0 {
        vm.tick()?;
      }
      let key = alloc_key(scope, &idx.to_string())?;
      let entry = vm.get_with_host_and_hooks(host, scope, hooks, obj, key)?;
      let Value::Object(entry_obj) = entry else {
        return Err(throw_type_error(vm, scope, host, hooks, "Invalid Headers init sequence item"));
      };
      if scope.heap().object_prototype(entry_obj)? != Some(array_proto) {
        return Err(throw_type_error(vm, scope, host, hooks, "Invalid Headers init sequence item"));
      }
      let entry_len_key = alloc_key(scope, "length")?;
      let entry_len = vm.get_with_host_and_hooks(host, scope, hooks, entry_obj, entry_len_key)?;
      let entry_len = number_to_u64(entry_len)?;
      if entry_len != 2 {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          hooks,
          "Invalid Headers init sequence item length",
        ));
      }
      let k0 = alloc_key(scope, "0")?;
      let k1 = alloc_key(scope, "1")?;
      let name_val = vm.get_with_host_and_hooks(host, scope, hooks, entry_obj, k0)?;
      let value_val = vm.get_with_host_and_hooks(host, scope, hooks, entry_obj, k1)?;
      let max_header_bytes = headers.limits().max_total_header_bytes;
      let name = to_rust_string_limited(
        scope.heap_mut(),
        name_val,
        max_header_bytes,
        FETCH_HEADER_NAME_TOO_LONG_ERROR,
      )?;
      let value = to_rust_string_limited(
        scope.heap_mut(),
        value_val,
        max_header_bytes,
        FETCH_HEADER_VALUE_TOO_LONG_ERROR,
      )?;
      sequence.push([name, value]);
    }
    headers
      .fill_from_sequence(&sequence)
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, hooks, err))?;
    return Ok(());
  }

  // Record form: iterate own keys in `[[OwnPropertyKeys]]` order.
  let keys = scope
    .heap()
    .ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
  let mut pairs: Vec<(String, String)> = Vec::new();
  pairs
    .try_reserve_exact(keys.len())
    .map_err(|_| VmError::OutOfMemory)?;

  const TICK_EVERY: usize = 256;
  for (i, key) in keys.into_iter().enumerate() {
    if i % TICK_EVERY == 0 {
      vm.tick()?;
    }
    let PropertyKey::String(s) = key else {
      continue;
    };
    let max_header_bytes = headers.limits().max_total_header_bytes;
    let name = js_string_to_rust_string_limited(
      scope.heap(),
      s,
      max_header_bytes,
      FETCH_HEADER_NAME_TOO_LONG_ERROR,
    )?;
    let value_val = vm.get_with_host_and_hooks(host, scope, hooks, obj, key)?;
    let value = to_rust_string_limited(
      scope.heap_mut(),
      value_val,
      max_header_bytes,
      FETCH_HEADER_VALUE_TOO_LONG_ERROR,
    )?;
    pairs.push((name, value));
  }
  headers
    .fill_from_pairs(pairs)
    .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, hooks, err))?;

  // Prevent unused warning for env_id (future: cross-env copy checks).
  let _ = env_id;
  Ok(())
}

fn headers_append_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let max_header_bytes = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.limits().max_total_header_bytes)
  })?;
  let name = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_NAME_TOO_LONG_ERROR,
  )?;
  let value = to_rust_string_limited(
    scope.heap_mut(),
    args.get(1).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_VALUE_TOO_LONG_ERROR,
  )?;

  with_env_state_mut(env_id, |state| {
    let headers = get_headers_mut(state, kind, owner)?;
    headers
      .append(&name, &value)
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, &mut *host, host_hooks, err))?;
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn headers_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let max_header_bytes = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.limits().max_total_header_bytes)
  })?;
  let name = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_NAME_TOO_LONG_ERROR,
  )?;
  let value = to_rust_string_limited(
    scope.heap_mut(),
    args.get(1).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_VALUE_TOO_LONG_ERROR,
  )?;

  with_env_state_mut(env_id, |state| {
    let headers = get_headers_mut(state, kind, owner)?;
    headers
      .set(&name, &value)
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, &mut *host, host_hooks, err))?;
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn headers_delete_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let max_header_bytes = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.limits().max_total_header_bytes)
  })?;
  let name = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_NAME_TOO_LONG_ERROR,
  )?;

  with_env_state_mut(env_id, |state| {
    let headers = get_headers_mut(state, kind, owner)?;
    headers
      .delete(&name)
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, &mut *host, host_hooks, err))?;
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn headers_has_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let max_header_bytes = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.limits().max_total_header_bytes)
  })?;
  let name = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_NAME_TOO_LONG_ERROR,
  )?;
  let has = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    headers
      .has(&name)
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))
  })?;
  Ok(Value::Bool(has))
}

fn headers_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let max_header_bytes = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.limits().max_total_header_bytes)
  })?;
  let name = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_NAME_TOO_LONG_ERROR,
  )?;
  let value = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    headers
      .get(&name)
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))
  })?;
  match value {
    Some(v) => {
      let s = scope.alloc_string(&v)?;
      Ok(Value::String(s))
    }
    None => Ok(Value::Null),
  }
}

fn headers_get_set_cookie_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let values = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.get_set_cookie())
  })?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let arr = scope.alloc_array(values.len())?;
  scope
    .heap_mut()
    .object_set_prototype(arr, Some(intr.array_prototype()))?;
  scope.push_root(Value::Object(arr))?;

  for (idx, value) in values.iter().enumerate() {
    let value_s = scope.alloc_string(value)?;
    // Root the element while allocating the property key: `alloc_key` can trigger GC.
    scope.push_root(Value::String(value_s))?;
    let key = alloc_key(scope, &idx.to_string())?;
    scope.define_property(arr, key, data_desc(Value::String(value_s), true))?;
  }

  Ok(Value::Object(arr))
}

fn headers_for_each_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);

  if !scope.heap().is_callable(callback).unwrap_or(false) {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Headers.forEach callback is not callable",
    ));
  }

  let pairs = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.sort_and_combine())
  })?;

  let Value::Object(headers_obj) = this else {
    return Err(VmError::TypeError("Headers: illegal invocation"));
  };

  for (name, value) in pairs {
    let value_s = scope.alloc_string(&value)?;
    scope.push_root(Value::String(value_s))?;
    let name_s = scope.alloc_string(&name)?;
    scope.push_root(Value::String(name_s))?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      callback,
      this_arg,
      &[Value::String(value_s), Value::String(name_s), Value::Object(headers_obj)],
    )?;
  }

  Ok(Value::Undefined)
}

fn headers_iter_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "fetch binding missing Headers iterator prototype native slot",
    )),
  }
}

fn headers_entries_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let iter_proto = headers_iter_proto_from_callee(scope, callee)?;
  let pairs = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.sort_and_combine())
  })?;
  let iter_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.headers_iterators.insert(
      id,
      HeadersIteratorState {
        pairs,
        index: 0,
      },
    );
    Ok(id)
  })?;

  let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
  scope.push_root(Value::Object(obj))?;
  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, HEADERS_ITER_ID_KEY, Value::Number(iter_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    HEADERS_ITER_KIND_KEY,
    Value::Number(HEADERS_ITER_KIND_ENTRIES as f64),
    false,
  )?;
  set_data_prop(scope, obj, HEADERS_ITER_DONE_KEY, Value::Bool(false), true)?;
  Ok(Value::Object(obj))
}

fn headers_keys_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let iter_proto = headers_iter_proto_from_callee(scope, callee)?;
  let pairs = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.sort_and_combine())
  })?;
  let iter_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.headers_iterators.insert(
      id,
      HeadersIteratorState {
        pairs,
        index: 0,
      },
    );
    Ok(id)
  })?;

  let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
  scope.push_root(Value::Object(obj))?;
  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, HEADERS_ITER_ID_KEY, Value::Number(iter_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    HEADERS_ITER_KIND_KEY,
    Value::Number(HEADERS_ITER_KIND_KEYS as f64),
    false,
  )?;
  set_data_prop(scope, obj, HEADERS_ITER_DONE_KEY, Value::Bool(false), true)?;
  Ok(Value::Object(obj))
}

fn headers_values_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, kind, owner) = headers_info_from_this(scope, this)?;
  let iter_proto = headers_iter_proto_from_callee(scope, callee)?;
  let pairs = with_env_state(env_id, |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.sort_and_combine())
  })?;
  let iter_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.headers_iterators.insert(
      id,
      HeadersIteratorState {
        pairs,
        index: 0,
      },
    );
    Ok(id)
  })?;

  let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
  scope.push_root(Value::Object(obj))?;
  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, HEADERS_ITER_ID_KEY, Value::Number(iter_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    HEADERS_ITER_KIND_KEY,
    Value::Number(HEADERS_ITER_KIND_VALUES as f64),
    false,
  )?;
  set_data_prop(scope, obj, HEADERS_ITER_DONE_KEY, Value::Bool(false), true)?;
  Ok(Value::Object(obj))
}

fn headers_iterator_next_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(iter_obj) = this else {
    return Err(VmError::TypeError("Headers iterator: illegal invocation"));
  };

  let done_val = get_data_prop(scope, iter_obj, HEADERS_ITER_DONE_KEY)?;
  let done = matches!(done_val, Value::Bool(true));

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let result_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(result_obj))?;

  if done {
    set_data_prop(scope, result_obj, "value", Value::Undefined, true)?;
    set_data_prop(scope, result_obj, "done", Value::Bool(true), true)?;
    return Ok(Value::Object(result_obj));
  }

  let env_val = get_data_prop(scope, iter_obj, ENV_ID_KEY)?;
  let iter_val = get_data_prop(scope, iter_obj, HEADERS_ITER_ID_KEY)?;
  let kind_val = get_data_prop(scope, iter_obj, HEADERS_ITER_KIND_KEY)?;
  if !matches!(env_val, Value::Number(_))
    || !matches!(iter_val, Value::Number(_))
    || !matches!(kind_val, Value::Number(_))
  {
    return Err(VmError::TypeError("Headers iterator: illegal invocation"));
  }
  let env_id = number_to_u64(env_val).map_err(|_| VmError::TypeError("Headers iterator: illegal invocation"))?;
  let iter_id = number_to_u64(iter_val).map_err(|_| VmError::TypeError("Headers iterator: illegal invocation"))?;
  let kind_u64 = number_to_u64(kind_val).map_err(|_| VmError::TypeError("Headers iterator: illegal invocation"))?;
  let kind: u8 = kind_u64
    .try_into()
    .map_err(|_| VmError::TypeError("Headers iterator: illegal invocation"))?;

  let next_pair: Option<(String, String)> = with_env_state_mut(env_id, |state| {
    let iter = state
      .headers_iterators
      .get_mut(&iter_id)
      .ok_or(VmError::TypeError("Headers iterator: invalid backing iterator"))?;
    if iter.index >= iter.pairs.len() {
      state.headers_iterators.remove(&iter_id);
      Ok(None)
    } else {
      let pair = iter
        .pairs
        .get(iter.index)
        .cloned()
        .ok_or(VmError::InvariantViolation("Headers iterator index out of bounds"))?;
      iter.index = iter.index.saturating_add(1);
      Ok(Some(pair))
    }
  })?;

  if let Some((name, value)) = next_pair {
    let out_value = match kind {
      HEADERS_ITER_KIND_ENTRIES => {
        let arr = scope.alloc_array(2)?;
        scope
          .heap_mut()
          .object_set_prototype(arr, Some(intr.array_prototype()))?;
        let name_s = scope.alloc_string(&name)?;
        let value_s = scope.alloc_string(&value)?;
        set_data_prop(scope, arr, "0", Value::String(name_s), true)?;
        set_data_prop(scope, arr, "1", Value::String(value_s), true)?;
        Value::Object(arr)
      }
      HEADERS_ITER_KIND_KEYS => {
        let name_s = scope.alloc_string(&name)?;
        Value::String(name_s)
      }
      HEADERS_ITER_KIND_VALUES => {
        let value_s = scope.alloc_string(&value)?;
        Value::String(value_s)
      }
      _ => return Err(VmError::TypeError("Headers iterator: illegal invocation")),
    };
    set_data_prop(scope, result_obj, "value", out_value, true)?;
    set_data_prop(scope, result_obj, "done", Value::Bool(false), true)?;
  } else {
    // Mark this iterator instance as done so subsequent `next()` calls don't throw if the
    // underlying env state has been cleaned up.
    set_data_prop(scope, iter_obj, HEADERS_ITER_DONE_KEY, Value::Bool(true), true)?;

    set_data_prop(scope, result_obj, "value", Value::Undefined, true)?;
    set_data_prop(scope, result_obj, "done", Value::Bool(true), true)?;
  }

  Ok(Value::Object(result_obj))
}

fn headers_iterator_iterator_native(
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

fn headers_ctor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(throw_type_error(
    vm,
    scope,
    host,
    host_hooks,
    "Illegal constructor",
  ))
}

fn headers_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let limits = with_env_state(env_id, |state| Ok(state.env.limits.clone()))?;

  let mut core = CoreHeaders::new_with_guard_and_limits(HeadersGuard::None, &limits);
  if let Some(init) = args.get(0).copied() {
    // Fill before installing into the env state so errors don't leave partial state behind.
    fill_headers_from_init(vm, scope, &mut *host, host_hooks, env_id, &mut core, init)?;
  }

  let headers_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.owned_headers.insert(id, core);
    Ok(id)
  })?;

  // Instance is a plain object with `Headers.prototype`.
  let proto = {
    let key = alloc_key(scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => return Err(VmError::InvariantViolation("Headers.prototype missing")),
    }
  };

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, HEADERS_KIND_KEY, Value::Number(HEADERS_KIND_OWNED as f64), false)?;
  set_data_prop(scope, obj, HEADERS_OWNER_KEY, Value::Number(headers_id as f64), false)?;

  Ok(Value::Object(obj))
}

fn escape_multipart_quoted_string_value(value: &str) -> String {
  // Avoid CRLF/header injection and keep quoting deterministic.
  let mut out = String::with_capacity(value.len());
  for ch in value.chars() {
    match ch {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\r' | '\n' => out.push(' '),
      other if other.is_control() => out.push(' '),
      other => out.push(other),
    }
  }
  out
}

fn push_bytes_limited(out: &mut Vec<u8>, bytes: &[u8], max_len: usize) -> Result<(), VmError> {
  let next_len = out.len().checked_add(bytes.len()).ok_or(VmError::OutOfMemory)?;
  if next_len > max_len {
    return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
  }
  out.try_reserve_exact(bytes.len()).map_err(|_| VmError::OutOfMemory)?;
  out.extend_from_slice(bytes);
  Ok(())
}

fn normalize_content_type_for_blob(header_value: &str) -> String {
  // Extract the MIME type "essence" (before `;`) and clamp to Blob's `type` semantics:
  // - ASCII lowercased
  // - empty string if it contains non-ASCII-printable characters
  let essence = header_value.split(';').next().unwrap_or("").trim();
  if essence.is_empty() {
    return String::new();
  }
  if !essence
    .as_bytes()
    .iter()
    .copied()
    .all(|b| (0x20..=0x7E).contains(&b))
  {
    return String::new();
  }
  essence.bytes().map(|b| (b as char).to_ascii_lowercase()).collect()
}

fn encode_form_data_as_multipart(
  entries: &[window_form_data::FormDataEntry],
  boundary: &str,
  max_len: usize,
) -> Result<Vec<u8>, VmError> {
  let mut out = Vec::<u8>::new();

  for entry in entries {
    push_bytes_limited(&mut out, b"--", max_len)?;
    push_bytes_limited(&mut out, boundary.as_bytes(), max_len)?;
    push_bytes_limited(&mut out, b"\r\n", max_len)?;

    let escaped_name = escape_multipart_quoted_string_value(&entry.name);
    match &entry.value {
      window_form_data::FormDataValue::String(value) => {
        let header = format!("Content-Disposition: form-data; name=\"{escaped_name}\"\r\n\r\n");
        push_bytes_limited(&mut out, header.as_bytes(), max_len)?;
        push_bytes_limited(&mut out, value.as_bytes(), max_len)?;
        push_bytes_limited(&mut out, b"\r\n", max_len)?;
      }
      window_form_data::FormDataValue::Blob { data, filename } => {
        let escaped_filename = escape_multipart_quoted_string_value(filename);
        let header = format!(
          "Content-Disposition: form-data; name=\"{escaped_name}\"; filename=\"{escaped_filename}\"\r\n"
        );
        push_bytes_limited(&mut out, header.as_bytes(), max_len)?;
        if !data.r#type.is_empty() {
          let content_type = format!("Content-Type: {}\r\n", data.r#type);
          push_bytes_limited(&mut out, content_type.as_bytes(), max_len)?;
        }
        push_bytes_limited(&mut out, b"\r\n", max_len)?;
        push_bytes_limited(&mut out, &data.bytes, max_len)?;
        push_bytes_limited(&mut out, b"\r\n", max_len)?;
      }
    }
  }

  push_bytes_limited(&mut out, b"--", max_len)?;
  push_bytes_limited(&mut out, boundary.as_bytes(), max_len)?;
  push_bytes_limited(&mut out, b"--\r\n", max_len)?;

  Ok(out)
}

fn unescape_multipart_quoted_string_value(value: &str) -> String {
  let mut out = String::with_capacity(value.len());
  let mut chars = value.chars();
  while let Some(ch) = chars.next() {
    if ch == '\\' {
      if let Some(next) = chars.next() {
        out.push(next);
      } else {
        out.push('\\');
      }
    } else {
      out.push(ch);
    }
  }
  out
}

fn parse_multipart_param_value(value: &str) -> String {
  let trimmed = value.trim();
  let Some(stripped) = trimmed
    .strip_prefix('"')
    .and_then(|s| s.strip_suffix('"'))
  else {
    return trimmed.to_string();
  };
  unescape_multipart_quoted_string_value(stripped)
}

fn extract_multipart_boundary(content_type: &str) -> Option<String> {
  for part in content_type.split(';').skip(1) {
    let part = part.trim();
    let Some(rest) = part.strip_prefix("boundary=").or_else(|| part.strip_prefix("Boundary=")) else {
      // ASCII case-insensitive match without allocating.
      if part.len() >= 9 && part.as_bytes()[..9].eq_ignore_ascii_case(b"boundary=") {
        let value = &part[9..];
        return Some(parse_multipart_param_value(value));
      }
      continue;
    };
    return Some(parse_multipart_param_value(rest));
  }
  None
}

fn find_subslice(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
  if needle.is_empty() {
    return Some(start.min(haystack.len()));
  }
  haystack
    .get(start..)
    .and_then(|slice| slice.windows(needle.len()).position(|w| w == needle))
    .map(|offset| start.saturating_add(offset))
}

fn parse_content_disposition_form_data(value: &str) -> Result<(String, Option<String>), &'static str> {
  let mut parts = value.split(';');
  let disposition = parts.next().unwrap_or("").trim();
  if !disposition.eq_ignore_ascii_case("form-data") {
    return Err("multipart/form-data part has invalid Content-Disposition");
  }

  let mut name: Option<String> = None;
  let mut filename: Option<String> = None;

  for part in parts {
    let part = part.trim();
    if part.is_empty() {
      continue;
    }
    let Some((k, v)) = part.split_once('=') else {
      continue;
    };
    let key = k.trim();
    let val = parse_multipart_param_value(v);
    if key.eq_ignore_ascii_case("name") {
      name = Some(val);
    } else if key.eq_ignore_ascii_case("filename") {
      filename = Some(val);
    }
  }

  let name = name.ok_or("multipart/form-data part missing name")?;
  Ok((name, filename))
}

fn parse_multipart_form_data(
  bytes: &[u8],
  boundary: &str,
) -> Result<Vec<window_form_data::FormDataEntry>, &'static str> {
  if boundary.is_empty() {
    return Err("multipart/form-data missing boundary");
  }

  let mut marker = Vec::<u8>::with_capacity(boundary.len().saturating_add(2));
  marker.extend_from_slice(b"--");
  marker.extend_from_slice(boundary.as_bytes());

  if !bytes.starts_with(&marker) {
    return Err("multipart/form-data body does not start with boundary");
  }

  let mut delimiter = Vec::<u8>::with_capacity(marker.len().saturating_add(2));
  delimiter.extend_from_slice(b"\r\n");
  delimiter.extend_from_slice(&marker);

  let mut pos = marker.len();
  let mut out: Vec<window_form_data::FormDataEntry> = Vec::new();

  loop {
    if pos > bytes.len() {
      return Err("multipart/form-data body is truncated");
    }

    if bytes.get(pos..pos + 2) == Some(b"--") {
      // Closing boundary (`--boundary--`). Ignore any epilogue.
      return Ok(out);
    }

    if bytes.get(pos..pos + 2) != Some(b"\r\n") {
      return Err("multipart/form-data boundary missing CRLF");
    }
    pos = pos.saturating_add(2);

    let headers_end = find_subslice(bytes, b"\r\n\r\n", pos).ok_or("multipart/form-data headers missing terminator")?;
    let headers_bytes = &bytes[pos..headers_end];
    pos = headers_end.saturating_add(4);

    let headers_str = String::from_utf8_lossy(headers_bytes);
    let mut disposition: Option<String> = None;
    let mut content_type: Option<String> = None;
    for line in headers_str.split("\r\n") {
      if line.is_empty() {
        continue;
      }
      let Some((name, value)) = line.split_once(':') else {
        continue;
      };
      let name = name.trim();
      let value = value.trim();
      if name.eq_ignore_ascii_case("content-disposition") {
        disposition = Some(value.to_string());
      } else if name.eq_ignore_ascii_case("content-type") {
        content_type = Some(value.to_string());
      }
    }

    let disposition = disposition.ok_or("multipart/form-data part missing Content-Disposition")?;
    let (field_name, filename) = parse_content_disposition_form_data(&disposition)?;

    let delimiter_pos = find_subslice(bytes, &delimiter, pos).ok_or("multipart/form-data part missing boundary")?;
    let part_bytes = &bytes[pos..delimiter_pos];
    pos = delimiter_pos.saturating_add(2); // Skip the leading CRLF.

    if !bytes.get(pos..).is_some_and(|b| b.starts_with(&marker)) {
      return Err("multipart/form-data boundary mismatch");
    }
    pos = pos.saturating_add(marker.len());

    let value = match filename {
      Some(filename) => {
        let r#type = content_type
          .as_deref()
          .map(normalize_content_type_for_blob)
          .unwrap_or_default();
        window_form_data::FormDataValue::Blob {
          data: window_blob::BlobData {
            bytes: part_bytes.to_vec(),
            r#type,
          },
          filename,
        }
      }
      None => window_form_data::FormDataValue::String(String::from_utf8_lossy(part_bytes).into_owned()),
    };

    out.push(window_form_data::FormDataEntry {
      name: field_name,
      value,
    });
  }
}

fn parse_urlencoded_form_data(bytes: &[u8]) -> Vec<window_form_data::FormDataEntry> {
  url::form_urlencoded::parse(bytes)
    .into_owned()
    .map(|(name, value)| window_form_data::FormDataEntry {
      name,
      value: window_form_data::FormDataValue::String(value),
    })
    .collect()
}

fn parse_form_data_entries_from_body(
  content_type: Option<&str>,
  bytes: &[u8],
) -> Result<Vec<window_form_data::FormDataEntry>, &'static str> {
  let content_type = content_type.ok_or("Body.formData requires a Content-Type header")?;
  let essence = normalize_content_type_for_blob(content_type);
  match essence.as_str() {
    "application/x-www-form-urlencoded" => Ok(parse_urlencoded_form_data(bytes)),
    "multipart/form-data" => {
      let boundary = extract_multipart_boundary(content_type).ok_or("multipart/form-data missing boundary")?;
      parse_multipart_form_data(bytes, &boundary)
    }
    _ => Err("Body.formData unsupported Content-Type"),
  }
}

fn apply_request_init(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  env_id: u64,
  limits: &WebFetchLimits,
  request: &mut CoreRequest,
  init: Value,
) -> Result<(), VmError> {
  if matches!(init, Value::Undefined | Value::Null) {
    return Ok(());
  }

  let Value::Object(init_obj) = init else {
    return Err(VmError::TypeError("Request init must be an object"));
  };

  // `mode` must be applied before headers so the correct guard is enforced when filling.
  let mode_key = alloc_key(scope, "mode")?;
  let mode_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, mode_key)?;
  let mut mode_changed = false;
  if !matches!(mode_val, Value::Undefined | Value::Null) {
    let mode_s = to_rust_string_limited(scope.heap_mut(), mode_val, 64, FETCH_MODE_TOO_LONG_ERROR)?;
    let mode = match mode_s.as_str() {
      "navigate" => RequestMode::Navigate,
      "same-origin" => RequestMode::SameOrigin,
      "no-cors" => RequestMode::NoCors,
      "cors" => RequestMode::Cors,
      _ => {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "Request.mode must be \"navigate\", \"same-origin\", \"no-cors\", or \"cors\"",
        ));
      }
    };
    if request.mode != mode {
      request.set_mode(mode);
      mode_changed = true;
    }
  }

  // `headers` replaces the existing header list.
  let headers_key = alloc_key(scope, "headers")?;
  let headers_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, headers_key)?;
  if !matches!(headers_val, Value::Undefined | Value::Null) {
    let mut headers = CoreHeaders::new_with_guard_and_limits(request.headers.guard(), request.headers.limits());
    fill_headers_from_init(vm, scope, host, host_hooks, env_id, &mut headers, headers_val)?;
    request.headers = headers;
  } else if mode_changed {
    // If mode changed (e.g. "cors" -> "no-cors"), re-apply the header list so any now-forbidden
    // headers are removed deterministically.
    let existing = request.headers.raw_pairs();
    let mut headers = CoreHeaders::new_with_guard_and_limits(request.headers.guard(), request.headers.limits());
    headers
      .fill_from_pairs(existing)
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
    request.headers = headers;
  }

  let method_key = alloc_key(scope, "method")?;
  let method_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, method_key)?;
  if !matches!(method_val, Value::Undefined | Value::Null) {
    let raw = to_rust_string_limited(
      scope.heap_mut(),
      method_val,
      limits.max_url_bytes,
      FETCH_METHOD_TOO_LONG_ERROR,
    )?;
    request.method = normalize_and_validate_method(vm, scope, host, host_hooks, &raw)?;
  } else {
    // Even when not overridden, normalize/validate so `new Request(req)` preserves browser casing.
    request.method =
      normalize_and_validate_method(vm, scope, host, host_hooks, request.method.as_str())?;
  }

  let redirect_key = alloc_key(scope, "redirect")?;
  let redirect_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, redirect_key)?;
  if !matches!(redirect_val, Value::Undefined | Value::Null) {
    let redirect_s = to_rust_string_limited(scope.heap_mut(), redirect_val, 64, FETCH_REDIRECT_TOO_LONG_ERROR)?;
    request.redirect = match redirect_s.as_str() {
      "follow" => RequestRedirect::Follow,
      "error" => RequestRedirect::Error,
      "manual" => RequestRedirect::Manual,
      _ => {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "Request.redirect must be \"follow\", \"error\", or \"manual\"",
        ));
      }
    };
  }

  let referrer_key = alloc_key(scope, "referrer")?;
  let referrer_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, referrer_key)?;
  if !matches!(referrer_val, Value::Undefined | Value::Null) {
    request.referrer = to_rust_string_limited(
      scope.heap_mut(),
      referrer_val,
      limits.max_url_bytes,
      FETCH_REFERRER_TOO_LONG_ERROR,
    )?;
  }

  let referrer_policy_key = alloc_key(scope, "referrerPolicy")?;
  let referrer_policy_val =
    vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, referrer_policy_key)?;
  if !matches!(referrer_policy_val, Value::Undefined | Value::Null) {
    let policy_s =
      to_rust_string_limited(scope.heap_mut(), referrer_policy_val, 64, FETCH_REFERRER_POLICY_TOO_LONG_ERROR)?;
    request.referrer_policy = ReferrerPolicy::parse(&policy_s).ok_or_else(|| {
      throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request.referrerPolicy must be a valid referrer policy token",
      )
    })?;
  }

  let credentials_key = alloc_key(scope, "credentials")?;
  let credentials_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, credentials_key)?;
  if !matches!(credentials_val, Value::Undefined | Value::Null) {
    let credentials = to_rust_string_limited(
      scope.heap_mut(),
      credentials_val,
      64,
      FETCH_CREDENTIALS_TOO_LONG_ERROR,
    )?;
    request.credentials = match credentials.as_str() {
      "omit" => RequestCredentials::Omit,
      "same-origin" => RequestCredentials::SameOrigin,
      "include" => RequestCredentials::Include,
      _ => {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "Request.credentials must be \"omit\", \"same-origin\", or \"include\"",
        ));
      }
    };
  }

  let body_key = alloc_key(scope, "body")?;
  let body_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, body_key)?;
  if !matches!(body_val, Value::Undefined | Value::Null) {
    let max_body_bytes = request.headers.limits().max_request_body_bytes;
    let mut inferred_content_type: Option<String> = None;

    let bytes: Vec<u8> = match body_val {
      Value::Object(obj) => {
        if scope.heap().is_array_buffer_object(obj) {
          let data = scope.heap().array_buffer_data(obj)?;
          if data.len() > max_body_bytes {
            return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
          }
          data.to_vec()
        } else if scope.heap().is_uint8_array_object(obj) {
          let data = scope.heap().uint8_array_data(obj)?;
          if data.len() > max_body_bytes {
            return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
          }
          data.to_vec()
        } else if let Some(serialized) =
          window_url::serialize_url_search_params_for_fetch(vm, scope.heap(), obj)?
        {
          if serialized.as_bytes().len() > max_body_bytes {
            return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
          }
          inferred_content_type =
            Some("application/x-www-form-urlencoded;charset=UTF-8".to_string());
          serialized.into_bytes()
        } else if let Some(blob) =
          window_blob::clone_blob_data_for_fetch(vm, scope.heap(), body_val)?
        {
          if blob.bytes.len() > max_body_bytes {
            return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
          }
          if !blob.r#type.is_empty() {
            inferred_content_type = Some(blob.r#type.clone());
          }
          blob.bytes
        } else if let Some(entries) =
          window_form_data::clone_form_data_entries_for_fetch(vm, scope.heap(), body_val)?
        {
          let boundary_id = with_env_state_mut(env_id, |state| {
            let id = state.multipart_boundary_counter;
            state.multipart_boundary_counter = state.multipart_boundary_counter.saturating_add(1);
            Ok(id)
          })?;
          let boundary = format!("----fastrenderformdata{boundary_id}");
          let multipart =
            encode_form_data_as_multipart(&entries, &boundary, max_body_bytes)?;
          inferred_content_type = Some(format!("multipart/form-data; boundary={boundary}"));
          multipart
        } else {
          let s = scope.to_string(vm, host, host_hooks, body_val)?;
          js_string_to_rust_string_limited(scope.heap(), s, max_body_bytes, FETCH_BODY_TOO_LONG_ERROR)?
            .into_bytes()
        }
      }
      other => {
        let s = scope.to_string(vm, host, host_hooks, other)?;
        js_string_to_rust_string_limited(scope.heap(), s, max_body_bytes, FETCH_BODY_TOO_LONG_ERROR)?
          .into_bytes()
      }
    };

    if let Some(content_type) = inferred_content_type {
      let has_content_type = request
        .headers
        .has("Content-Type")
        .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
      if !has_content_type {
        request
          .headers
          .set("Content-Type", &content_type)
          .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
      }
    }

    let body = Body::new_with_limits(bytes, request.headers.limits())
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
    request.body = Some(body);
  }

  // Fetch invariants.
  if request.method.eq_ignore_ascii_case("GET") || request.method.eq_ignore_ascii_case("HEAD") {
    if request.body.is_some() {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request body is not allowed for GET/HEAD",
      ));
    }
  }

  if request.mode == RequestMode::NoCors {
    if request.redirect != RequestRedirect::Follow {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request.redirect must be \"follow\" for mode \"no-cors\"",
      ));
    }
    if !(request.method.eq_ignore_ascii_case("GET")
      || request.method.eq_ignore_ascii_case("HEAD")
      || request.method.eq_ignore_ascii_case("POST"))
    {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request.mode \"no-cors\" requires a CORS-safelisted method (GET/HEAD/POST)",
      ));
    }
  }

  Ok(())
}

fn request_ctor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(throw_type_error(
    vm,
    scope,
    host,
    host_hooks,
    "Illegal constructor",
  ))
}

fn request_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;
  let limits = with_env_state(env_id, |state| Ok(state.env.limits.clone()))?;
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let init = args.get(1).copied().unwrap_or(Value::Undefined);

  let input_request_info = request_info_from_value(scope, input);
  let input_request_obj = match (input_request_info, input) {
    (Some(_), Value::Object(obj)) => Some(obj),
    _ => None,
  };

  let mut request = if let Some((other_env_id, other_request_id)) = input_request_info {
    with_env_state(other_env_id, |state| {
      state
        .requests
        .get(&other_request_id)
        .ok_or(VmError::TypeError("Request: invalid backing request"))
        .and_then(|req| {
          if req.body.as_ref().map_or(false, |b| b.body_used()) {
            return Err(throw_type_error(
              vm,
              scope,
              host,
              host_hooks,
              "Request body is already used",
            ));
          }
          Ok(req.clone())
        })
    })?
  } else {
    let url = to_rust_string_limited(
      scope.heap_mut(),
      input,
      limits.max_url_bytes,
      FETCH_URL_TOO_LONG_ERROR,
    )?;
    let base_url = current_document_base_url(vm, env_id)?;
    let url = resolve_url(&url, base_url.as_deref())
      .map_err(|err| throw_type_error(vm, scope, host, host_hooks, &err.to_string()))?;
    CoreRequest::new_with_limits("GET", url, &limits)
  };

  // Associated AbortSignal (optional; FastRender currently treats missing signals as `null`).
  let mut signal: Option<Value> = None;
  let mut init_specified_signal = false;

  apply_request_init(vm, scope, host, host_hooks, env_id, &limits, &mut request, init)?;

  // Enforce invariants even when `init` is omitted (e.g. `new Request(existingRequest)`).
  request.method =
    normalize_and_validate_method(vm, scope, host, host_hooks, request.method.as_str())?;
  if request.method.eq_ignore_ascii_case("GET") || request.method.eq_ignore_ascii_case("HEAD") {
    if request.body.is_some() {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request body is not allowed for GET/HEAD",
      ));
    }
  }
  if request.mode == RequestMode::NoCors {
    if request.redirect != RequestRedirect::Follow {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request.redirect must be \"follow\" for mode \"no-cors\"",
      ));
    }
    if !(request.method.eq_ignore_ascii_case("GET")
      || request.method.eq_ignore_ascii_case("HEAD")
      || request.method.eq_ignore_ascii_case("POST"))
    {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request.mode \"no-cors\" requires a CORS-safelisted method (GET/HEAD/POST)",
      ));
    }
  }

  // Resolve the associated AbortSignal, if any.
  //
  // `new Request(input, init)` matches the `fetch(input, init)` behavior: an explicit `init.signal`
  // overrides `input.signal` when `input` is a `Request`.
  if !matches!(init, Value::Undefined | Value::Null) {
    // `apply_request_init` already validated `init` is an object when present; keep a defensive
    // check here to preserve VM invariants.
    let Value::Object(init_obj) = init else {
      return Err(VmError::InvariantViolation("Request init must be an object"));
    };
    let signal_key = alloc_key(scope, "signal")?;
    let signal_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, signal_key)?;
    if !matches!(signal_val, Value::Undefined) {
      init_specified_signal = true;
      match signal_val {
        Value::Undefined | Value::Null => signal = None,
        Value::Object(_) => signal = Some(signal_val),
        _ => {
          return Err(throw_type_error(
            vm,
            scope,
            host,
            host_hooks,
            "RequestInit.signal must be an AbortSignal or null",
          ));
        }
      }
    }
  }

  if !init_specified_signal {
    if let Some(input_obj) = input_request_obj {
      let inherited = get_data_prop(scope, input_obj, "signal")?;
      if matches!(inherited, Value::Object(_)) {
        signal = Some(inherited);
      }
    }
  }

  let request_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.requests.insert(id, request);
    Ok(id)
  })?;

  // Instance object.
  let proto = {
    let key = alloc_key(scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => return Err(VmError::InvariantViolation("Request.prototype missing")),
    }
  };
  let obj = make_request_wrapper(scope, env_id, headers_proto, proto, request_id)?;
  set_data_prop(
    scope,
    obj,
    "signal",
    signal.unwrap_or(Value::Null),
    /* writable */ false,
  )?;
  Ok(Value::Object(obj))
}

fn request_clone_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(original_obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(original_obj))?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;
  let signal = match get_data_prop(scope, original_obj, "signal")? {
    Value::Undefined => Value::Null,
    other => other,
  };

  let cloned = with_env_state(env_id, |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    if req.body.as_ref().map_or(false, |b| b.body_used()) {
      return Err(throw_type_error(
        vm,
        scope,
        &mut *host,
        host_hooks,
        "Request body is already used",
      ));
    }
    Ok(req.clone())
  })?;

  let new_request_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.requests.insert(id, cloned);
    Ok(id)
  })?;

  let proto = scope
    .heap()
    .object_prototype(original_obj)?
    .ok_or(VmError::InvariantViolation(
    "Request.prototype missing on instance",
  ))?;
  let obj = make_request_wrapper(scope, env_id, headers_proto, proto, new_request_id)?;
  set_data_prop(scope, obj, "signal", signal, /* writable */ false)?;
  Ok(Value::Object(obj))
}

fn request_text_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, request_id) = request_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let result: std::result::Result<String, WebFetchError> = with_env_state_mut(env_id, |state| {
    let req = state
      .requests
      .get_mut(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    let result = match req.body.as_mut() {
      Some(body) => body.text_utf8(),
      None => Ok(String::new()),
    };
    Ok(result)
  })?;

  match result {
    Ok(text) => {
      let s = scope.alloc_string(&text)?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.resolve,
        Value::Undefined,
        &[Value::String(s)],
      )?;
    }
    Err(err) => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn request_array_buffer_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, request_id) = request_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let result: std::result::Result<Vec<u8>, WebFetchError> = with_env_state_mut(env_id, |state| {
    let req = state
      .requests
      .get_mut(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    let result = match req.body.as_mut() {
      Some(body) => body.consume_bytes(),
      None => Ok(Vec::new()),
    };
    Ok(result)
  })?;

  match result {
    Ok(bytes) => {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.resolve,
        Value::Undefined,
        &[Value::Object(ab)],
      )?;
    }
    Err(err) => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn request_blob_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, request_id) = request_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let (bytes_result, content_type_result): (
    std::result::Result<Vec<u8>, WebFetchError>,
    std::result::Result<Option<String>, WebFetchError>,
  ) = with_env_state_mut(env_id, |state| {
    let req = state
      .requests
      .get_mut(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    let bytes_result = match req.body.as_mut() {
      Some(body) => body.consume_bytes(),
      None => Ok(Vec::new()),
    };
    let content_type_result = req.headers.get("Content-Type");
    Ok((bytes_result, content_type_result))
  })?;

  match (bytes_result, content_type_result) {
    (Ok(bytes), Ok(content_type)) => {
      let blob_type = content_type
        .as_deref()
        .map(normalize_content_type_for_blob)
        .unwrap_or_default();

      let blob_result = (|| -> Result<GcObject, VmError> {
        let realm_id = vm.current_realm().ok_or(VmError::Unimplemented(
          "Request.blob requires an active realm",
        ))?;
        let proto = window_blob::blob_prototype_for_realm(realm_id)
          .ok_or(VmError::Unimplemented("Request.blob requires Blob to be installed"))?;
        window_blob::create_blob_with_proto(
          vm,
          scope,
          callee,
          proto,
          window_blob::BlobData { bytes, r#type: blob_type },
        )
      })();

      match blob_result {
        Ok(blob_obj) => {
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.resolve,
            Value::Undefined,
            &[Value::Object(blob_obj)],
          )?;
        }
        Err(err) => {
          let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.reject,
            Value::Undefined,
            &[err_value],
          )?;
        }
      }
    }
    (Err(err), _) | (_, Err(err)) => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn request_form_data_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, request_id) = request_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let (bytes_result, content_type_result): (
    std::result::Result<Vec<u8>, WebFetchError>,
    std::result::Result<Option<String>, WebFetchError>,
  ) = with_env_state_mut(env_id, |state| {
    let req = state
      .requests
      .get_mut(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    let bytes_result = match req.body.as_mut() {
      Some(body) => body.consume_bytes(),
      None => Ok(Vec::new()),
    };
    let content_type_result = req.headers.get("Content-Type");
    Ok((bytes_result, content_type_result))
  })?;

  match (bytes_result, content_type_result) {
    (Ok(bytes), Ok(content_type)) => {
      let entries = parse_form_data_entries_from_body(content_type.as_deref(), &bytes);
      match entries {
        Ok(entries) => {
          let form_data_result = window_form_data::create_form_data_with_entries(vm, scope, callee, entries);
          match form_data_result {
            Ok(fd_obj) => {
              vm.call_with_host_and_hooks(
                &mut *host,
                scope,
                host_hooks,
                cap.resolve,
                Value::Undefined,
                &[Value::Object(fd_obj)],
              )?;
            }
            Err(err) => {
              let err_value = match err {
                VmError::TypeError(msg) => create_type_error(vm, scope, &mut *host, host_hooks, msg)?,
                other => {
                  let msg = other.to_string();
                  create_type_error(vm, scope, &mut *host, host_hooks, &msg)?
                }
              };
              vm.call_with_host_and_hooks(
                &mut *host,
                scope,
                host_hooks,
                cap.reject,
                Value::Undefined,
                &[err_value],
              )?;
            }
          }
        }
        Err(msg) => {
          let err_value = create_type_error(vm, scope, &mut *host, host_hooks, msg)?;
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.reject,
            Value::Undefined,
            &[err_value],
          )?;
        }
      }
    }
    (Err(err), _) | (_, Err(err)) => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn request_json_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, request_id) = request_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let parsed: Option<std::result::Result<serde_json::Value, WebFetchError>> =
    with_env_state_mut(env_id, |state| {
      let req = state
        .requests
        .get_mut(&request_id)
        .ok_or(VmError::TypeError("Request: invalid backing request"))?;
      let parsed = req.body.as_mut().map(|body| body.json());
      Ok(parsed)
    })?;

  match parsed {
    Some(Ok(value)) => {
      let js_value = json_to_js(vm, scope, &value)?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.resolve,
        Value::Undefined,
        &[js_value],
      )?;
    }
    Some(Err(err)) => match err {
      WebFetchError::BodyInvalidJson(e) => {
        let err_value = create_syntax_error(vm, scope, &mut *host, host_hooks, &e.to_string())?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
      }
      other => {
        let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &other.to_string())?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
      }
    },
    None => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Request body is null")?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn request_body_used_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, request_id) = request_info_from_this(scope, this)?;
  let used = with_env_state(env_id, |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    Ok(req.body.as_ref().map_or(false, |b| b.body_used()))
  })?;
  Ok(Value::Bool(used))
}

fn response_ctor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(throw_type_error(
    vm,
    scope,
    host,
    host_hooks,
    "Illegal constructor",
  ))
}

fn response_error_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;
  let response_proto = response_proto_from_callee(scope, callee)?;

  let mut response = CoreResponse::new(0);
  response.r#type = crate::resource::web_fetch::ResponseType::Error;
  response.headers.set_guard(HeadersGuard::Immutable);

  let response_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.responses.insert(id, response);
    Ok(id)
  })?;

  let resp_obj = make_response_wrapper(scope, env_id, headers_proto, response_proto, response_id)?;
  Ok(Value::Object(resp_obj))
}

fn response_redirect_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;
  let response_proto = response_proto_from_callee(scope, callee)?;

  let max_url_bytes = with_env_state(env_id, |state| Ok(state.env.limits.max_url_bytes))?;
  let url_input = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_url_bytes,
    FETCH_URL_TOO_LONG_ERROR,
  )?;
  let base_url = current_document_base_url(vm, env_id)?;
  let resolved_url = match resolve_url(&url_input, base_url.as_deref()) {
    Ok(url) => url,
    Err(UrlResolveError::RelativeUrlWithoutBase) => {
      return Err(throw_type_error(
        vm,
        scope,
        &mut *host,
        host_hooks,
        "Response.redirect URL is relative without a base URL",
      ));
    }
    Err(UrlResolveError::Url(_)) => {
      return Err(throw_type_error(
        vm,
        scope,
        &mut *host,
        host_hooks,
        "Response.redirect URL is invalid",
      ));
    }
  };

  let status_val = args.get(1).copied().unwrap_or(Value::Number(302.0));
  let status_num = scope.heap_mut().to_number(status_val)?;
  let status = number_to_u16_wrapping(status_num);
  if !matches!(status, 301 | 302 | 303 | 307 | 308) {
    return Err(throw_range_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Response.redirect status must be a redirect status",
    ));
  }

  // Build headers while mutable, then lock them down to match the "immutable" guard.
  let mut headers = CoreHeaders::new_with_guard(HeadersGuard::Response);
  headers
    .append("Location", &resolved_url)
    .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
  headers.set_guard(HeadersGuard::Immutable);

  let mut response = CoreResponse::new(status);
  response.headers = headers;

  let response_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.responses.insert(id, response);
    Ok(id)
  })?;

  let resp_obj = make_response_wrapper(scope, env_id, headers_proto, response_proto, response_id)?;
  Ok(Value::Object(resp_obj))
}

fn response_json_static_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;
  let response_proto = response_proto_from_callee(scope, callee)?;
  let limits = with_env_state(env_id, |state| Ok(state.env.limits.clone()))?;

  let data = args.get(0).copied().unwrap_or(Value::Undefined);
  let init = args.get(1).copied().unwrap_or(Value::Undefined);

  // WHATWG Fetch: `Response.json(data, init)`
  // https://fetch.spec.whatwg.org/#dom-response-json
  //
  // Step 1: serialize a JavaScript value to JSON bytes.
  // This relies on the realm's `JSON.stringify` implementation; if it returns `undefined`, the
  // Infra algorithm specifies treating it as `"null"`.
  let json_bytes = {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    let json = intr.json();

    // Root `json` and `data` while allocating property keys and calling into JS: any allocation can
    // trigger GC.
    let mut call_scope = scope.reborrow();
    call_scope.push_root(Value::Object(json))?;
    call_scope.push_root(data)?;
    let stringify_key = alloc_key(&mut call_scope, "stringify")?;
    let stringify_fn = vm.get_with_host_and_hooks(&mut *host, &mut call_scope, host_hooks, json, stringify_key)?;

    let result = vm.call_with_host_and_hooks(
      &mut *host,
      &mut call_scope,
      host_hooks,
      stringify_fn,
      Value::Object(json),
      &[data],
    )?;

    let serialized = match result {
      Value::Undefined => "null".to_string(),
      Value::String(s) => js_string_to_rust_string_limited(
        call_scope.heap(),
        s,
        limits.max_response_body_bytes,
        FETCH_BODY_TOO_LONG_ERROR,
      )?,
      _ => return Err(VmError::InvariantViolation("JSON.stringify returned non-string")),
    };

    serialized.into_bytes()
  };

  // Step 2: extract the bytes as a BodyInit. `Body::new_response` enforces response body limits.
  let body = crate::resource::web_fetch::Body::new_response(json_bytes, &limits)
    .map_err(|e| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, e))?;

  // Step 3/4: create the response and initialize it with `init` and the `(body, "application/json")` type.
  let mut status: u16 = 200;
  let mut status_text = String::new();
  let mut headers = CoreHeaders::new_with_guard_and_limits(HeadersGuard::Response, &limits);

  if !matches!(init, Value::Undefined | Value::Null) {
    let Value::Object(init_obj) = init else {
      return Err(VmError::TypeError("Response init must be an object"));
    };
    let status_key = alloc_key(scope, "status")?;
    let status_val = vm.get_with_host_and_hooks(&mut *host, scope, host_hooks, init_obj, status_key)?;
    if !matches!(status_val, Value::Undefined) {
      let n = scope.heap_mut().to_number(status_val)?;
      status = number_to_u16_wrapping(n);
    }
    let status_text_key = alloc_key(scope, "statusText")?;
    let st_val = vm.get_with_host_and_hooks(&mut *host, scope, host_hooks, init_obj, status_text_key)?;
    if !matches!(st_val, Value::Undefined) {
      status_text = to_rust_string_limited(
        scope.heap_mut(),
        st_val,
        limits.max_url_bytes,
        FETCH_STATUS_TEXT_TOO_LONG_ERROR,
      )?;
    }
    let headers_key = alloc_key(scope, "headers")?;
    let headers_val = vm.get_with_host_and_hooks(&mut *host, scope, host_hooks, init_obj, headers_key)?;
    if !matches!(headers_val, Value::Undefined | Value::Null) {
      fill_headers_from_init(vm, scope, &mut *host, host_hooks, env_id, &mut headers, headers_val)?;
    }
  }

  if !(200..=599).contains(&status) {
    return Err(throw_range_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Response status must be in range 200 to 599, inclusive",
    ));
  }
  if !status_text.is_empty() && !is_reason_phrase_byte_string(&status_text) {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Response statusText must be a valid reason phrase",
    ));
  }
  if matches!(status, 101 | 103 | 204 | 205 | 304) {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Response cannot have a body with a null body status",
    ));
  }

  // `initialize a response` appends the content-type if not already present.
  if !headers
    .has("Content-Type")
    .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?
  {
    headers
      .append("Content-Type", "application/json")
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
  }

  let mut response = CoreResponse::new(status);
  response.status_text = status_text;
  response.headers = headers;
  response.body = Some(body);

  let response_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.responses.insert(id, response);
    Ok(id)
  })?;

  let resp_obj = make_response_wrapper(scope, env_id, headers_proto, response_proto, response_id)?;
  Ok(Value::Object(resp_obj))
}

fn response_text_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, response_id) = response_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let result: std::result::Result<String, WebFetchError> = with_env_state_mut(env_id, |state| {
    let res = state
      .responses
      .get_mut(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    let result = match res.body.as_mut() {
      Some(body) => body.text_utf8(),
      None => Ok(String::new()),
    };
    Ok(result)
  })?;

  match result {
    Ok(text) => {
      let s = scope.alloc_string(&text)?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.resolve,
        Value::Undefined,
        &[Value::String(s)],
      )?;
    }
    Err(err) => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn response_array_buffer_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, response_id) = response_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let result: std::result::Result<Vec<u8>, WebFetchError> = with_env_state_mut(env_id, |state| {
    let res = state
      .responses
      .get_mut(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    let result = match res.body.as_mut() {
      Some(body) => body.consume_bytes(),
      None => Ok(Vec::new()),
    };
    Ok(result)
  })?;

  match result {
    Ok(bytes) => {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.resolve,
        Value::Undefined,
        &[Value::Object(ab)],
      )?;
    }
    Err(err) => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn response_blob_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, response_id) = response_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let (bytes_result, content_type_result): (
    std::result::Result<Vec<u8>, WebFetchError>,
    std::result::Result<Option<String>, WebFetchError>,
  ) = with_env_state_mut(env_id, |state| {
    let res = state
      .responses
      .get_mut(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    let bytes_result = match res.body.as_mut() {
      Some(body) => body.consume_bytes(),
      None => Ok(Vec::new()),
    };
    let content_type_result = res.headers.get("Content-Type");
    Ok((bytes_result, content_type_result))
  })?;

  match (bytes_result, content_type_result) {
    (Ok(bytes), Ok(content_type)) => {
      let blob_type = content_type
        .as_deref()
        .map(normalize_content_type_for_blob)
        .unwrap_or_default();

      let blob_result = (|| -> Result<GcObject, VmError> {
        let realm_id = vm.current_realm().ok_or(VmError::Unimplemented(
          "Response.blob requires an active realm",
        ))?;
        let proto = window_blob::blob_prototype_for_realm(realm_id)
          .ok_or(VmError::Unimplemented("Response.blob requires Blob to be installed"))?;
        window_blob::create_blob_with_proto(
          vm,
          scope,
          callee,
          proto,
          window_blob::BlobData { bytes, r#type: blob_type },
        )
      })();

      match blob_result {
        Ok(blob_obj) => {
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.resolve,
            Value::Undefined,
            &[Value::Object(blob_obj)],
          )?;
        }
        Err(err) => {
          let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.reject,
            Value::Undefined,
            &[err_value],
          )?;
        }
      }
    }
    (Err(err), _) | (_, Err(err)) => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn response_form_data_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, response_id) = response_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let (bytes_result, content_type_result): (
    std::result::Result<Vec<u8>, WebFetchError>,
    std::result::Result<Option<String>, WebFetchError>,
  ) = with_env_state_mut(env_id, |state| {
    let res = state
      .responses
      .get_mut(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    let bytes_result = match res.body.as_mut() {
      Some(body) => body.consume_bytes(),
      None => Ok(Vec::new()),
    };
    let content_type_result = res.headers.get("Content-Type");
    Ok((bytes_result, content_type_result))
  })?;

  match (bytes_result, content_type_result) {
    (Ok(bytes), Ok(content_type)) => {
      let entries = parse_form_data_entries_from_body(content_type.as_deref(), &bytes);
      match entries {
        Ok(entries) => {
          let form_data_result = window_form_data::create_form_data_with_entries(vm, scope, callee, entries);
          match form_data_result {
            Ok(fd_obj) => {
              vm.call_with_host_and_hooks(
                &mut *host,
                scope,
                host_hooks,
                cap.resolve,
                Value::Undefined,
                &[Value::Object(fd_obj)],
              )?;
            }
            Err(err) => {
              let err_value = match err {
                VmError::TypeError(msg) => create_type_error(vm, scope, &mut *host, host_hooks, msg)?,
                other => {
                  let msg = other.to_string();
                  create_type_error(vm, scope, &mut *host, host_hooks, &msg)?
                }
              };
              vm.call_with_host_and_hooks(
                &mut *host,
                scope,
                host_hooks,
                cap.reject,
                Value::Undefined,
                &[err_value],
              )?;
            }
          }
        }
        Err(msg) => {
          let err_value = create_type_error(vm, scope, &mut *host, host_hooks, msg)?;
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.reject,
            Value::Undefined,
            &[err_value],
          )?;
        }
      }
    }
    (Err(err), _) | (_, Err(err)) => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn json_to_js(vm: &mut Vm, scope: &mut Scope<'_>, value: &serde_json::Value) -> Result<Value, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  match value {
    serde_json::Value::Null => Ok(Value::Null),
    serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
    serde_json::Value::Number(n) => Ok(Value::Number(n.as_f64().unwrap_or(f64::NAN))),
    serde_json::Value::String(s) => {
      let js = scope.alloc_string(s)?;
      Ok(Value::String(js))
    }
    serde_json::Value::Array(items) => {
      let arr = scope.alloc_array(items.len())?;
      scope
        .heap_mut()
        .object_set_prototype(arr, Some(intr.array_prototype()))?;
      scope.push_root(Value::Object(arr))?;
      for (idx, item) in items.iter().enumerate() {
        let key = alloc_key(scope, &idx.to_string())?;
        let js_value = json_to_js(vm, scope, item)?;
        scope.define_property(arr, key, data_desc(js_value, true))?;
      }
      Ok(Value::Object(arr))
    }
    serde_json::Value::Object(map) => {
      let obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
      scope.push_root(Value::Object(obj))?;
      for (k, v) in map {
        let key = alloc_key(scope, k)?;
        let js_value = json_to_js(vm, scope, v)?;
        scope.define_property(obj, key, data_desc(js_value, true))?;
      }
      Ok(Value::Object(obj))
    }
  }
}

fn response_json_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, response_id) = response_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  let parsed: Option<std::result::Result<serde_json::Value, WebFetchError>> =
    with_env_state_mut(env_id, |state| {
    let res = state
      .responses
      .get_mut(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    let parsed = res.body.as_mut().map(|body| body.json());
    Ok(parsed)
  })?;

  match parsed {
    Some(Ok(value)) => {
      let js_value = json_to_js(vm, scope, &value)?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.resolve,
        Value::Undefined,
        &[js_value],
      )?;
    }
    Some(Err(err)) => match err {
      WebFetchError::BodyInvalidJson(e) => {
        let err_value = create_syntax_error(vm, scope, &mut *host, host_hooks, &e.to_string())?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
      }
      other => {
        let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &other.to_string())?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
      }
    },
    None => {
      let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Response body is null")?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
    }
  }

  Ok(cap.promise)
}

fn response_clone_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  let (env_id, response_id) = response_info_from_this(scope, Value::Object(obj))?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;

  let cloned = with_env_state(env_id, |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    if res.body.as_ref().map_or(false, |b| b.body_used()) {
      return Err(throw_type_error(
        vm,
        scope,
        &mut *host,
        host_hooks,
        "Response body is already used",
      ));
    }
    Ok(res.clone())
  })?;

  let new_response_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.responses.insert(id, cloned);
    Ok(id)
  })?;

  let proto = scope.heap().object_prototype(obj)?.ok_or(VmError::InvariantViolation(
    "Response.prototype missing on instance",
  ))?;
  let resp_obj = make_response_wrapper(scope, env_id, headers_proto, proto, new_response_id)?;
  Ok(Value::Object(resp_obj))
}

fn response_body_used_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, response_id) = response_info_from_this(scope, this)?;
  let used = with_env_state(env_id, |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    Ok(res.body.as_ref().map_or(false, |b| b.body_used()))
  })?;
  Ok(Value::Bool(used))
}

fn make_headers_wrapper(
  scope: &mut Scope<'_>,
  env_id: u64,
  headers_proto: GcObject,
  kind: u8,
  owner: u64,
) -> Result<GcObject, VmError> {
  let obj = scope.alloc_object_with_prototype(Some(headers_proto))?;
  scope.push_root(Value::Object(obj))?;
  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, HEADERS_KIND_KEY, Value::Number(kind as f64), false)?;
  set_data_prop(scope, obj, HEADERS_OWNER_KEY, Value::Number(owner as f64), false)?;
  Ok(obj)
}

fn make_request_wrapper(
  scope: &mut Scope<'_>,
  env_id: u64,
  headers_proto: GcObject,
  request_proto: GcObject,
  request_id: u64,
) -> Result<GcObject, VmError> {
  let obj = scope.alloc_object_with_prototype(Some(request_proto))?;
  scope.push_root(Value::Object(obj))?;

  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, REQUEST_ID_KEY, Value::Number(request_id as f64), false)?;

  let (method, url, mode, credentials, redirect, referrer, referrer_policy) = with_env_state(env_id, |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::InvariantViolation("Request state missing"))?;
    Ok((
      req.method.clone(),
      req.url.clone(),
      req.mode,
      req.credentials,
      req.redirect,
      req.referrer.clone(),
      req.referrer_policy,
    ))
  })?;

  let method_s = scope.alloc_string(&method)?;
  let url_s = scope.alloc_string(&url)?;
  set_data_prop(scope, obj, "method", Value::String(method_s), false)?;
  set_data_prop(scope, obj, "url", Value::String(url_s), false)?;

  let mode_s = scope.alloc_string(request_mode_to_string(mode))?;
  set_data_prop(scope, obj, "mode", Value::String(mode_s), false)?;
  let credentials_s = scope.alloc_string(request_credentials_to_string(credentials))?;
  set_data_prop(scope, obj, "credentials", Value::String(credentials_s), false)?;
  let redirect_s = scope.alloc_string(request_redirect_to_string(redirect))?;
  set_data_prop(scope, obj, "redirect", Value::String(redirect_s), false)?;

  let referrer_s = scope.alloc_string(&referrer)?;
  set_data_prop(scope, obj, "referrer", Value::String(referrer_s), false)?;
  let referrer_policy_s = scope.alloc_string(referrer_policy.as_str())?;
  set_data_prop(scope, obj, "referrerPolicy", Value::String(referrer_policy_s), false)?;

  let headers_obj = make_headers_wrapper(scope, env_id, headers_proto, HEADERS_KIND_REQUEST, request_id)?;
  set_data_prop(scope, obj, "headers", Value::Object(headers_obj), false)?;

  Ok(obj)
}

fn make_response_wrapper(
  scope: &mut Scope<'_>,
  env_id: u64,
  headers_proto: GcObject,
  response_proto: GcObject,
  response_id: u64,
) -> Result<GcObject, VmError> {
  let obj = scope.alloc_object_with_prototype(Some(response_proto))?;
  scope.push_root(Value::Object(obj))?;

  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, RESPONSE_ID_KEY, Value::Number(response_id as f64), false)?;

  let (status, url, status_text, r#type, redirected) = with_env_state(env_id, |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::InvariantViolation("Response state missing"))?;
    Ok((
      res.status,
      res.url.clone(),
      res.status_text.clone(),
      res.r#type,
      res.redirected,
    ))
  })?;

  let ok = (200..300).contains(&status);
  set_data_prop(scope, obj, "status", Value::Number(status as f64), false)?;
  set_data_prop(scope, obj, "ok", Value::Bool(ok), false)?;
  let url_s = scope.alloc_string(&url)?;
  let st_s = scope.alloc_string(&status_text)?;
  set_data_prop(scope, obj, "url", Value::String(url_s), false)?;
  set_data_prop(scope, obj, "statusText", Value::String(st_s), false)?;
  let type_s = scope.alloc_string(response_type_to_string(r#type))?;
  set_data_prop(scope, obj, "type", Value::String(type_s), false)?;
  set_data_prop(scope, obj, "redirected", Value::Bool(redirected), false)?;

  let headers_obj = make_headers_wrapper(scope, env_id, headers_proto, HEADERS_KIND_RESPONSE, response_id)?;
  set_data_prop(scope, obj, "headers", Value::Object(headers_obj), false)?;

  Ok(obj)
}

fn response_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;
  let limits = with_env_state(env_id, |state| Ok(state.env.limits.clone()))?;

  let init = args.get(1).copied().unwrap_or(Value::Undefined);
  let mut status: u16 = 200;
  let mut status_text = String::new();
  let mut headers = CoreHeaders::new_with_guard_and_limits(HeadersGuard::Response, &limits);

  let body = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut body_bytes: Option<Vec<u8>> = None;
  let mut inferred_content_type: Option<String> = None;
  if !matches!(body, Value::Undefined | Value::Null) {
    let max_body_bytes = headers.limits().max_response_body_bytes;
    let bytes: Vec<u8> = match body {
      Value::Object(obj) => {
        if scope.heap().is_array_buffer_object(obj) {
          let data = scope.heap().array_buffer_data(obj)?;
          if data.len() > max_body_bytes {
            return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
          }
          data.to_vec()
        } else if scope.heap().is_uint8_array_object(obj) {
          let data = scope.heap().uint8_array_data(obj)?;
          if data.len() > max_body_bytes {
            return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
          }
          data.to_vec()
        } else if let Some(serialized) =
          window_url::serialize_url_search_params_for_fetch(vm, scope.heap(), obj)?
        {
          if serialized.as_bytes().len() > max_body_bytes {
            return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
          }
          inferred_content_type = Some("application/x-www-form-urlencoded;charset=UTF-8".to_string());
          serialized.into_bytes()
        } else if let Some(blob) = window_blob::clone_blob_data_for_fetch(vm, scope.heap(), body)? {
          if blob.bytes.len() > max_body_bytes {
            return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
          }
          if !blob.r#type.is_empty() {
            inferred_content_type = Some(blob.r#type.clone());
          }
          blob.bytes
        } else if let Some(entries) =
          window_form_data::clone_form_data_entries_for_fetch(vm, scope.heap(), body)?
        {
          let boundary_id = with_env_state_mut(env_id, |state| {
            let id = state.multipart_boundary_counter;
            state.multipart_boundary_counter = state.multipart_boundary_counter.saturating_add(1);
            Ok(id)
          })?;
          let boundary = format!("----fastrenderformdata{boundary_id}");
          let multipart = encode_form_data_as_multipart(&entries, &boundary, max_body_bytes)?;
          inferred_content_type = Some(format!("multipart/form-data; boundary={boundary}"));
          multipart
        } else {
          let s = scope.to_string(vm, host, host_hooks, body)?;
          js_string_to_rust_string_limited(scope.heap(), s, max_body_bytes, FETCH_BODY_TOO_LONG_ERROR)?
            .into_bytes()
        }
      }
      other => {
        let s = scope.to_string(vm, host, host_hooks, other)?;
        js_string_to_rust_string_limited(scope.heap(), s, max_body_bytes, FETCH_BODY_TOO_LONG_ERROR)?
          .into_bytes()
      }
    };
    body_bytes = Some(bytes);
  }

  if !matches!(init, Value::Undefined | Value::Null) {
    let Value::Object(init_obj) = init else {
      return Err(VmError::TypeError("Response init must be an object"));
    };
    let status_key = alloc_key(scope, "status")?;
    let status_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, status_key)?;
    if !matches!(status_val, Value::Undefined) {
      let n = scope.heap_mut().to_number(status_val)?;
      status = number_to_u16_wrapping(n);
    }
    let status_text_key = alloc_key(scope, "statusText")?;
    let st_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, status_text_key)?;
    if !matches!(st_val, Value::Undefined) {
      status_text = to_rust_string_limited(
        scope.heap_mut(),
        st_val,
        limits.max_url_bytes,
        FETCH_STATUS_TEXT_TOO_LONG_ERROR,
      )?;
    }
    let headers_key = alloc_key(scope, "headers")?;
    let headers_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, headers_key)?;
    if !matches!(headers_val, Value::Undefined | Value::Null) {
      fill_headers_from_init(vm, scope, &mut *host, host_hooks, env_id, &mut headers, headers_val)?;
    }
  }

  if !(200..=599).contains(&status) {
    return Err(throw_range_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Response status must be in range 200 to 599, inclusive",
    ));
  }
  if !status_text.is_empty() && !is_reason_phrase_byte_string(&status_text) {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Response statusText must be a valid reason phrase",
    ));
  }
  if body_bytes.is_some() && matches!(status, 101 | 103 | 204 | 205 | 304) {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Response cannot have a body with a null body status",
    ));
  }

  if let Some(content_type) = inferred_content_type {
    let has_content_type = headers
      .has("Content-Type")
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
    if !has_content_type {
      headers
        .append("Content-Type", &content_type)
        .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
    }
  }

  let mut response = CoreResponse::new(status);
  response.status_text = status_text;
  response.headers = headers;
  if let Some(bytes) = body_bytes {
    response.body = Some(
      crate::resource::web_fetch::Body::new_response(bytes, response.headers.limits())
        .map_err(|e| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, e))?,
    );
  }

  let response_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.responses.insert(id, response);
    Ok(id)
  })?;

  let proto = {
    let key = alloc_key(scope, "prototype")?;
    match scope
      .heap()
      .object_get_own_data_property_value(callee, &key)?
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => obj,
      _ => return Err(VmError::InvariantViolation("Response.prototype missing")),
    }
  };
  let obj = make_response_wrapper(scope, env_id, headers_proto, proto, response_id)?;
  Ok(Value::Object(obj))
}

fn fetch_call<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;
  let response_proto = response_proto_from_callee(scope, callee)?;
  let limits = with_env_state(env_id, |state| Ok(state.env.limits.clone()))?;
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let init = args.get(1).copied().unwrap_or(Value::Undefined);

  // Build request synchronously (invalid init should reject deterministically).
  let input_request_info = request_info_from_value(scope, input);
  let input_request_obj = match (input_request_info, input) {
    (Some(_), Value::Object(obj)) => Some(obj),
    _ => None,
  };

  let mut request = if let Some((other_env_id, other_request_id)) = input_request_info {
    with_env_state(other_env_id, |state| {
      state
        .requests
        .get(&other_request_id)
        .ok_or(VmError::TypeError("Request: invalid backing request"))
        .and_then(|req| {
          if req.body.as_ref().map_or(false, |b| b.body_used()) {
            return Err(throw_type_error(
              vm,
              scope,
              host,
              host_hooks,
              "Request body is already used",
            ));
          }
          Ok(req.clone())
        })
    })?
  } else {
    let url = to_rust_string_limited(
      scope.heap_mut(),
      input,
      limits.max_url_bytes,
      FETCH_URL_TOO_LONG_ERROR,
    )?;
    let base_url = current_document_base_url(vm, env_id)?;
    let url = resolve_url(&url, base_url.as_deref())
      .map_err(|err| throw_type_error(vm, scope, host, host_hooks, &err.to_string()))?;
    CoreRequest::new_with_limits("GET", url, &limits)
  };

  apply_request_init(vm, scope, host, host_hooks, env_id, &limits, &mut request, init)?;

  // Enforce invariants even when `init` is omitted (e.g. `fetch(existingRequest)`).
  request.method =
    normalize_and_validate_method(vm, scope, host, host_hooks, request.method.as_str())?;
  if request.method.eq_ignore_ascii_case("GET") || request.method.eq_ignore_ascii_case("HEAD") {
    if request.body.is_some() {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request body is not allowed for GET/HEAD",
      ));
    }
  }
  if request.mode == RequestMode::NoCors {
    if request.redirect != RequestRedirect::Follow {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request.redirect must be \"follow\" for mode \"no-cors\"",
      ));
    }
    if !(request.method.eq_ignore_ascii_case("GET")
      || request.method.eq_ignore_ascii_case("HEAD")
      || request.method.eq_ignore_ascii_case("POST"))
    {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request.mode \"no-cors\" requires a CORS-safelisted method (GET/HEAD/POST)",
      ));
    }
  }

  // Resolve the associated AbortSignal, if any.
  //
  // `fetch(input, init)` matches the `new Request(input, init)` behavior: an explicit `init.signal`
  // overrides `input.signal` when `input` is a `Request`.
  let mut signal: Option<Value> = None;
  let mut init_specified_signal = false;

  if !matches!(init, Value::Undefined | Value::Null) {
    // `apply_request_init` already validated `init` is an object when present; keep a defensive
    // check here to preserve VM invariants.
    let Value::Object(init_obj) = init else {
      return Err(VmError::InvariantViolation("Request init must be an object"));
    };
    let signal_key = alloc_key(scope, "signal")?;
    let signal_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, signal_key)?;
    if !matches!(signal_val, Value::Undefined) {
      init_specified_signal = true;
      match signal_val {
        Value::Undefined | Value::Null => signal = None,
        Value::Object(_) => signal = Some(signal_val),
        _ => {
          return Err(throw_type_error(
            vm,
            scope,
            host,
            host_hooks,
            "RequestInit.signal must be an AbortSignal or null",
          ));
        }
      }
    }
  }

  if !init_specified_signal {
    if let Some(input_obj) = input_request_obj {
      let inherited = get_data_prop(scope, input_obj, "signal")?;
      if matches!(inherited, Value::Object(_)) {
        signal = Some(inherited);
      }
    }
  }

  // Create a Promise capability for the returned Promise.
  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
  let promise = cap.promise;

  // Resolve/reject later; keep them rooted until settlement.
  let resolve_root = scope.heap_mut().add_root(cap.resolve)?;
  let reject_root = scope.heap_mut().add_root(cap.reject)?;
  let promise_root = scope.heap_mut().add_root(promise)?;
  let signal_root = match signal {
    Some(v) => Some(scope.heap_mut().add_root(v)?),
    None => None,
  };

  // If the signal is already aborted, reject immediately and skip queueing any networking task.
  if let Some(signal_root) = signal_root {
    let signal_value = scope
      .heap()
      .get_root(signal_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    if let Value::Object(signal_obj) = signal_value {
      let aborted_key = alloc_key(scope, "aborted")?;
      let aborted = vm.get_with_host_and_hooks(host, scope, host_hooks, signal_obj, aborted_key)?;
      if scope.heap().to_boolean(aborted)? {
        let reason_key = alloc_key(scope, "reason")?;
        let reason = vm.get_with_host_and_hooks(host, scope, host_hooks, signal_obj, reason_key)?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[reason],
        )?;
        scope.heap_mut().remove_root(resolve_root);
        scope.heap_mut().remove_root(reject_root);
        scope.heap_mut().remove_root(promise_root);
        scope.heap_mut().remove_root(signal_root);
        return Ok(promise);
      }
    }
  }

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(host_hooks) else {
    // Reject synchronously.
    scope.heap_mut().remove_root(resolve_root);
    scope.heap_mut().remove_root(reject_root);
    scope.heap_mut().remove_root(promise_root);
    if let Some(signal_root) = signal_root {
      scope.heap_mut().remove_root(signal_root);
    }
    let err = create_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "fetch called without an active EventLoop",
    )?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err],
    )?;
    return Ok(promise);
  };

  let enqueue_result = event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
    // Execute `execute_web_fetch` synchronously on this networking task.
    let (fetcher, document_url, document_origin, referrer_policy) = match with_env_state(env_id, |state| {
      let env = &state.env;
      Ok((
        Arc::clone(&env.fetcher),
        env.document_url.clone(),
        env.document_origin.clone(),
        env.referrer_policy,
      ))
    }) {
      Ok(tuple) => tuple,
      Err(err) => {
        let message = format!("fetch failed: {err}");
        let queue_result = event_loop.queue_microtask(move |host, event_loop| {
          let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
          hooks.set_event_loop(event_loop);
          let (vm_host, window_realm) = host.vm_host_and_window_realm();
          window_realm.reset_interrupt();
          let budget = window_realm.vm_budget_now();
          let (vm, heap) = window_realm.vm_and_heap_mut();
          let mut vm = vm.push_budget(budget);
          let tick_result = vm.tick();
          let call_result = tick_result.and_then(|_| {
            let reject = heap
              .get_root(reject_root)
              .ok_or_else(|| VmError::invalid_handle())?;
            let mut scope = heap.scope();
            let type_error =
              create_type_error(&mut vm, &mut scope, vm_host, &mut hooks, &message)?;
            vm.call_with_host_and_hooks(
              vm_host,
              &mut scope,
              &mut hooks,
              reject,
              Value::Undefined,
              &[type_error],
            )?;
            Ok(())
          });

          // Remove roots even if the rejection throws/terminates.
          heap.remove_root(resolve_root);
          heap.remove_root(reject_root);
          heap.remove_root(promise_root);
          if let Some(signal_root) = signal_root {
            heap.remove_root(signal_root);
          }
 
          if let Some(err) = hooks.finish(heap) {
            return Err(err);
          }
          call_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ())
        });

        if let Err(queue_err) = queue_result {
          // If we can't even enqueue the rejection microtask, tear down persistent roots now.
          let window_realm = host.window_realm();
          window_realm.heap_mut().remove_root(resolve_root);
          window_realm.heap_mut().remove_root(reject_root);
          window_realm.heap_mut().remove_root(promise_root);
          if let Some(signal_root) = signal_root {
            window_realm.heap_mut().remove_root(signal_root);
          }
          return Err(queue_err);
        }

        return Ok(());
      }
    };

    // If the signal was aborted after `fetch()` returned but before this networking task begins,
    // reject and skip executing the underlying fetcher.
    if let Some(signal_root_id) = signal_root {
      let window_realm = host.window_realm();
      let aborted = (|| {
        let heap = window_realm.heap_mut();
        let signal_value = heap.get_root(signal_root_id)?;
        let Value::Object(signal_obj) = signal_value else {
          return None;
        };
        let mut scope = heap.scope();
        let key = alloc_key(&mut scope, "aborted").ok()?;
        let value = scope
          .heap()
          .object_get_own_data_property_value(signal_obj, &key)
          .ok()
          .flatten()
          .unwrap_or(Value::Undefined);
        scope.heap().to_boolean(value).ok()
      })();

      if aborted.unwrap_or(false) {
        let queue_result = event_loop.queue_microtask(move |host, event_loop| {
          let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
          hooks.set_event_loop(event_loop);
          let (vm_host, window_realm) = host.vm_host_and_window_realm();
          window_realm.reset_interrupt();
          let budget = window_realm.vm_budget_now();
          let (vm, heap) = window_realm.vm_and_heap_mut();
          let mut vm = vm.push_budget(budget);
          let tick_result = vm.tick();
          let call_result = tick_result.and_then(|_| {
            let reject = heap
              .get_root(reject_root)
              .ok_or_else(|| VmError::invalid_handle())?;
            let signal_value = heap
              .get_root(signal_root_id)
              .ok_or_else(|| VmError::invalid_handle())?;
            let mut scope = heap.scope();
            let reason = match signal_value {
              Value::Object(signal_obj) => {
                let key = alloc_key(&mut scope, "reason")?;
                vm.get_with_host_and_hooks(vm_host, &mut scope, &mut hooks, signal_obj, key)?
              }
              _ => Value::Undefined,
            };
            vm.call_with_host_and_hooks(
              vm_host,
              &mut scope,
              &mut hooks,
              reject,
              Value::Undefined,
              &[reason],
            )?;
            Ok(())
          });

          heap.remove_root(resolve_root);
          heap.remove_root(reject_root);
          heap.remove_root(promise_root);
          heap.remove_root(signal_root_id);

          if let Some(err) = hooks.finish(heap) {
            return Err(err);
          }
          call_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ())
        });

        if let Err(queue_err) = queue_result {
          let window_realm = host.window_realm();
          window_realm.heap_mut().remove_root(resolve_root);
          window_realm.heap_mut().remove_root(reject_root);
          window_realm.heap_mut().remove_root(promise_root);
          window_realm.heap_mut().remove_root(signal_root_id);
          return Err(queue_err);
        }

        return Ok(());
      }
    }

    let exec_ctx = WebFetchExecutionContext {
      destination: FetchDestination::Fetch,
      referrer_url: document_url.as_deref(),
      client_origin: document_origin.as_ref(),
      referrer_policy,
      csp: None,
    };

    let result = execute_web_fetch(fetcher.as_ref(), &request, exec_ctx);

    match result {
      Ok(mut response) => {
        // JS `Response.headers` for fetch() results is immutable in browsers.
        response.headers.set_guard(HeadersGuard::Immutable);

            let response_id = match with_env_state_mut(env_id, |state| {
              let id = state.alloc_id();
              state.responses.insert(id, response);
              Ok(id)
            }) {
              Ok(id) => id,
              Err(err) => {
                let message = format!("fetch failed: {err}");
                let queue_result = event_loop.queue_microtask(move |host, event_loop| {
                  let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
                  hooks.set_event_loop(event_loop);
                  let (vm_host, window_realm) = host.vm_host_and_window_realm();
                  window_realm.reset_interrupt();
                  let budget = window_realm.vm_budget_now();
                  let (vm, heap) = window_realm.vm_and_heap_mut();
                  let mut vm = vm.push_budget(budget);
                  let tick_result = vm.tick();
                  let call_result = tick_result.and_then(|_| {
                    let reject = heap
                      .get_root(reject_root)
                      .ok_or_else(|| VmError::invalid_handle())?;
                    let mut scope = heap.scope();
                    let type_error =
                      create_type_error(&mut vm, &mut scope, vm_host, &mut hooks, &message)?;
                    vm.call_with_host_and_hooks(
                      vm_host,
                      &mut scope,
                      &mut hooks,
                      reject,
                      Value::Undefined,
                      &[type_error],
                    )?;
                    Ok(())
                  });

                  heap.remove_root(resolve_root);
                  heap.remove_root(reject_root);
                  heap.remove_root(promise_root);
              if let Some(signal_root) = signal_root {
                heap.remove_root(signal_root);
              }

              if let Some(err) = hooks.finish(heap) {
                return Err(err);
              }
              call_result
                .map_err(|err| vm_error_to_event_loop_error(heap, err))
                .map(|_| ())
            });

            if let Err(queue_err) = queue_result {
              let window_realm = host.window_realm();
              window_realm.heap_mut().remove_root(resolve_root);
              window_realm.heap_mut().remove_root(reject_root);
              window_realm.heap_mut().remove_root(promise_root);
              if let Some(signal_root) = signal_root {
                window_realm.heap_mut().remove_root(signal_root);
              }
              return Err(queue_err);
            }

            return Ok(());
          }
        };

        let queue_result = event_loop.queue_microtask(move |host, event_loop| {
          // Resolve the promise with a JS Response wrapper.
          let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
          let (vm_host, window_realm) = host.vm_host_and_window_realm();
          hooks.set_event_loop(event_loop);
          window_realm.reset_interrupt();
          let budget = window_realm.vm_budget_now();
          let (vm, heap) = window_realm.vm_and_heap_mut();
          let mut vm = vm.push_budget(budget);
          let tick_result = vm.tick();

          let call_result = tick_result.and_then(|_| {
            let resolve = heap
              .get_root(resolve_root)
              .ok_or_else(|| VmError::invalid_handle())?;
            let mut scope = heap.scope();

            let resp_obj =
              make_response_wrapper(&mut scope, env_id, headers_proto, response_proto, response_id)?;

            // Call resolve(responseObj) with host hooks so Promise jobs are enqueued onto the
            // EventLoop microtask queue.
            vm.call_with_host_and_hooks(
              vm_host,
              &mut scope,
              &mut hooks,
              resolve,
              Value::Undefined,
              &[Value::Object(resp_obj)],
            )?;
            Ok(())
          });

          // Remove roots even if resolution fails.
          heap.remove_root(resolve_root);
          heap.remove_root(reject_root);
          heap.remove_root(promise_root);
          if let Some(signal_root) = signal_root {
            heap.remove_root(signal_root);
          }

          if let Some(err) = hooks.finish(heap) {
            return Err(err);
          }
          call_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ())
        });

        if let Err(queue_err) = queue_result {
          // Failed to enqueue the resolve microtask; tear down persistent roots now.
          let _ = with_env_state_mut(env_id, |state| {
            state.responses.remove(&response_id);
            Ok(())
          });
          let window_realm = host.window_realm();
          window_realm.heap_mut().remove_root(resolve_root);
          window_realm.heap_mut().remove_root(reject_root);
          window_realm.heap_mut().remove_root(promise_root);
          if let Some(signal_root) = signal_root {
            window_realm.heap_mut().remove_root(signal_root);
          }
          return Err(queue_err);
        }
      }
      Err(err) => {
        let message = format!("fetch failed: {err}");
        let queue_result = event_loop.queue_microtask(move |host, event_loop| {
          let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host);
          hooks.set_event_loop(event_loop);
          let (vm_host, window_realm) = host.vm_host_and_window_realm();
          window_realm.reset_interrupt();
          let budget = window_realm.vm_budget_now();
          let (vm, heap) = window_realm.vm_and_heap_mut();
          let mut vm = vm.push_budget(budget);
          let tick_result = vm.tick();
          let call_result = tick_result.and_then(|_| {
            let reject = heap
              .get_root(reject_root)
              .ok_or_else(|| VmError::invalid_handle())?;
            let mut scope = heap.scope();
            let type_error =
              create_type_error(&mut vm, &mut scope, vm_host, &mut hooks, &message)?;
            vm.call_with_host_and_hooks(
              vm_host,
              &mut scope,
              &mut hooks,
              reject,
              Value::Undefined,
              &[type_error],
            )?;
            Ok(())
          });

          heap.remove_root(resolve_root);
          heap.remove_root(reject_root);
          heap.remove_root(promise_root);
          if let Some(signal_root) = signal_root {
            heap.remove_root(signal_root);
          }

          if let Some(err) = hooks.finish(heap) {
            return Err(err);
          }
          call_result
            .map_err(|err| vm_error_to_event_loop_error(heap, err))
            .map(|_| ())
        });

        if let Err(queue_err) = queue_result {
          let window_realm = host.window_realm();
          window_realm.heap_mut().remove_root(resolve_root);
          window_realm.heap_mut().remove_root(reject_root);
          window_realm.heap_mut().remove_root(promise_root);
          if let Some(signal_root) = signal_root {
            window_realm.heap_mut().remove_root(signal_root);
          }
          return Err(queue_err);
        }
      }
    }

    Ok(())
  });

  if let Err(err) = enqueue_result {
    // Failed to enqueue networking task; reject synchronously and clean up roots.
    scope.heap_mut().remove_root(resolve_root);
    scope.heap_mut().remove_root(reject_root);
    scope.heap_mut().remove_root(promise_root);
    if let Some(signal_root) = signal_root {
      scope.heap_mut().remove_root(signal_root);
    }
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
  }

  Ok(promise)
}

/// Install Fetch bindings onto the window global object.
///
/// Returns an env id that can be passed to [`unregister_window_fetch_env`] to tear down the backing
/// Rust state when the realm/host is dropped.
pub fn install_window_fetch_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowFetchEnv,
) -> Result<u64, VmError> {
  let bindings = install_window_fetch_bindings_with_guard::<Host>(vm, realm, heap, env)?;
  Ok(bindings.disarm())
}

/// Install Fetch bindings onto the window global object, returning an RAII guard that automatically
/// unregisters the backing Rust state when dropped.
pub fn install_window_fetch_bindings_with_guard<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowFetchEnv,
) -> Result<WindowFetchBindings, VmError> {
  let env_id = NEXT_ENV_ID.fetch_add(1, Ordering::Relaxed);
  let promise_executor_call = vm.register_native_call(promise_capability_executor_call)?;
  {
    let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.insert(env_id, EnvState::new(env, promise_executor_call));
  }

  let bindings = WindowFetchBindings::new(env_id);

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let func_proto = realm.intrinsics().function_prototype();

  // --- Headers ---
  let headers_proto = {
    let call_id = vm.register_native_call(headers_ctor_call)?;
    let construct_id = vm.register_native_construct(headers_ctor_construct)?;
    let name_s = scope.alloc_string("Headers")?;
    scope.push_root(Value::String(name_s))?;
    let ctor = scope.alloc_native_function_with_slots(
      call_id,
      Some(construct_id),
      name_s,
      1,
      &[Value::Number(env_id as f64)],
    )?;
    scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;
    scope.push_root(Value::Object(ctor))?;

    // Install prototype methods.
    let Value::Object(proto) = get_data_prop(&mut scope, ctor, "prototype")? else {
      return Err(VmError::InvariantViolation("Headers.prototype missing"));
    };
    scope.push_root(Value::Object(proto))?;

    let append_id = vm.register_native_call(headers_append_native)?;
    let append_name = scope.alloc_string("append")?;
    scope.push_root(Value::String(append_name))?;
    let append = scope.alloc_native_function(append_id, None, append_name, 2)?;
    scope.heap_mut().object_set_prototype(append, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "append", Value::Object(append), true)?;

    let set_id = vm.register_native_call(headers_set_native)?;
    let set_name = scope.alloc_string("set")?;
    scope.push_root(Value::String(set_name))?;
    let set_fn = scope.alloc_native_function(set_id, None, set_name, 2)?;
    scope.heap_mut().object_set_prototype(set_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "set", Value::Object(set_fn), true)?;

    let get_id = vm.register_native_call(headers_get_native)?;
    let get_name = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_name))?;
    let get_fn = scope.alloc_native_function(get_id, None, get_name, 1)?;
    scope.heap_mut().object_set_prototype(get_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "get", Value::Object(get_fn), true)?;

    let get_set_cookie_id = vm.register_native_call(headers_get_set_cookie_native)?;
    let get_set_cookie_name = scope.alloc_string("getSetCookie")?;
    scope.push_root(Value::String(get_set_cookie_name))?;
    let get_set_cookie_fn = scope.alloc_native_function(get_set_cookie_id, None, get_set_cookie_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(get_set_cookie_fn, Some(func_proto))?;
    set_data_prop(
      &mut scope,
      proto,
      "getSetCookie",
      Value::Object(get_set_cookie_fn),
      true,
    )?;

    let has_id = vm.register_native_call(headers_has_native)?;
    let has_name = scope.alloc_string("has")?;
    scope.push_root(Value::String(has_name))?;
    let has_fn = scope.alloc_native_function(has_id, None, has_name, 1)?;
    scope.heap_mut().object_set_prototype(has_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "has", Value::Object(has_fn), true)?;

    let delete_id = vm.register_native_call(headers_delete_native)?;
    let delete_name = scope.alloc_string("delete")?;
    scope.push_root(Value::String(delete_name))?;
    let delete_fn = scope.alloc_native_function(delete_id, None, delete_name, 1)?;
    scope.heap_mut().object_set_prototype(delete_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "delete", Value::Object(delete_fn), true)?;

    let for_each_id = vm.register_native_call(headers_for_each_native)?;
    let for_each_name = scope.alloc_string("forEach")?;
    scope.push_root(Value::String(for_each_name))?;
    let for_each_fn = scope.alloc_native_function(for_each_id, None, for_each_name, 1)?;
    scope.heap_mut().object_set_prototype(for_each_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "forEach", Value::Object(for_each_fn), true)?;

    // Deterministic iteration for `Headers` (`entries`/`keys`/`values` + @@iterator).
    let iter_proto = {
      let object_proto = realm.intrinsics().object_prototype();
      let iter_proto = scope.alloc_object_with_prototype(Some(object_proto))?;
      scope.push_root(Value::Object(iter_proto))?;

      let next_id = vm.register_native_call(headers_iterator_next_native)?;
      let next_name = scope.alloc_string("next")?;
      scope.push_root(Value::String(next_name))?;
      let next_fn = scope.alloc_native_function(next_id, None, next_name, 0)?;
      scope.heap_mut().object_set_prototype(next_fn, Some(func_proto))?;
      set_data_prop(&mut scope, iter_proto, "next", Value::Object(next_fn), true)?;

      let iter_id = vm.register_native_call(headers_iterator_iterator_native)?;
      let iter_name = scope.alloc_string("Symbol.iterator")?;
      scope.push_root(Value::String(iter_name))?;
      let iter_fn = scope.alloc_native_function(iter_id, None, iter_name, 0)?;
      scope.push_root(Value::Object(iter_fn))?;
      scope.heap_mut().object_set_prototype(iter_fn, Some(func_proto))?;
      let sym_key = alloc_symbol_key(&mut scope, "Symbol.iterator")?;
      scope.define_property(iter_proto, sym_key, data_desc(Value::Object(iter_fn), true))?;

      iter_proto
    };

    let entries_id = vm.register_native_call(headers_entries_native)?;
    let entries_name = scope.alloc_string("entries")?;
    scope.push_root(Value::String(entries_name))?;
    let entries_fn = scope.alloc_native_function_with_slots(
      entries_id,
      None,
      entries_name,
      0,
      &[Value::Object(iter_proto)],
    )?;
    scope
      .heap_mut()
      .object_set_prototype(entries_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "entries", Value::Object(entries_fn), true)?;

    let keys_id = vm.register_native_call(headers_keys_native)?;
    let keys_name = scope.alloc_string("keys")?;
    scope.push_root(Value::String(keys_name))?;
    let keys_fn = scope.alloc_native_function_with_slots(
      keys_id,
      None,
      keys_name,
      0,
      &[Value::Object(iter_proto)],
    )?;
    scope.heap_mut().object_set_prototype(keys_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "keys", Value::Object(keys_fn), true)?;

    let values_id = vm.register_native_call(headers_values_native)?;
    let values_name = scope.alloc_string("values")?;
    scope.push_root(Value::String(values_name))?;
    let values_fn = scope.alloc_native_function_with_slots(
      values_id,
      None,
      values_name,
      0,
      &[Value::Object(iter_proto)],
    )?;
    scope
      .heap_mut()
      .object_set_prototype(values_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "values", Value::Object(values_fn), true)?;

    // `[Symbol.iterator]` is an alias for `entries()`.
    let sym_key = alloc_symbol_key(&mut scope, "Symbol.iterator")?;
    scope.define_property(proto, sym_key, data_desc(Value::Object(entries_fn), true))?;

    // Define global.
    let key = alloc_key(&mut scope, "Headers")?;
    scope.define_property(global, key, data_desc(Value::Object(ctor), true))?;
    proto
  };

  // --- Request ---
  {
    let call_id = vm.register_native_call(request_ctor_call)?;
    let construct_id = vm.register_native_construct(request_ctor_construct)?;
    let name_s = scope.alloc_string("Request")?;
    scope.push_root(Value::String(name_s))?;
    let ctor = scope.alloc_native_function_with_slots(
      call_id,
      Some(construct_id),
      name_s,
      2,
      &[Value::Number(env_id as f64), Value::Object(headers_proto)],
    )?;
    scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;
    scope.push_root(Value::Object(ctor))?;

    // Prototype methods.
    let Value::Object(proto) = get_data_prop(&mut scope, ctor, "prototype")? else {
      return Err(VmError::InvariantViolation("Request.prototype missing"));
    };
    scope.push_root(Value::Object(proto))?;

    let clone_id = vm.register_native_call(request_clone_native)?;
    let clone_name = scope.alloc_string("clone")?;
    scope.push_root(Value::String(clone_name))?;
    let clone_fn = scope.alloc_native_function_with_slots(
      clone_id,
      None,
      clone_name,
      0,
      &[Value::Number(env_id as f64), Value::Object(headers_proto)],
    )?;
    scope.heap_mut().object_set_prototype(clone_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "clone", Value::Object(clone_fn), true)?;

    let text_id = vm.register_native_call(request_text_native)?;
    let text_name = scope.alloc_string("text")?;
    scope.push_root(Value::String(text_name))?;
    let text_fn = scope.alloc_native_function(text_id, None, text_name, 0)?;
    scope.heap_mut().object_set_prototype(text_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "text", Value::Object(text_fn), true)?;

    let json_id = vm.register_native_call(request_json_native)?;
    let json_name = scope.alloc_string("json")?;
    scope.push_root(Value::String(json_name))?;
    let json_fn = scope.alloc_native_function(json_id, None, json_name, 0)?;
    scope.heap_mut().object_set_prototype(json_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "json", Value::Object(json_fn), true)?;

    let array_buffer_id = vm.register_native_call(request_array_buffer_native)?;
    let array_buffer_name = scope.alloc_string("arrayBuffer")?;
    scope.push_root(Value::String(array_buffer_name))?;
    let array_buffer_fn = scope.alloc_native_function(array_buffer_id, None, array_buffer_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(array_buffer_fn, Some(func_proto))?;
    set_data_prop(
      &mut scope,
      proto,
      "arrayBuffer",
      Value::Object(array_buffer_fn),
      true,
    )?;

    let blob_id = vm.register_native_call(request_blob_native)?;
    let blob_name = scope.alloc_string("blob")?;
    scope.push_root(Value::String(blob_name))?;
    let blob_fn = scope.alloc_native_function(blob_id, None, blob_name, 0)?;
    scope.heap_mut().object_set_prototype(blob_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "blob", Value::Object(blob_fn), true)?;

    let form_data_id = vm.register_native_call(request_form_data_native)?;
    let form_data_name = scope.alloc_string("formData")?;
    scope.push_root(Value::String(form_data_name))?;
    let form_data_fn = scope.alloc_native_function(form_data_id, None, form_data_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(form_data_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "formData", Value::Object(form_data_fn), true)?;

    // bodyUsed accessor (getter only).
    let body_used_get_id = vm.register_native_call(request_body_used_get_native)?;
    let body_used_get_name = scope.alloc_string("get bodyUsed")?;
    scope.push_root(Value::String(body_used_get_name))?;
    let body_used_get = scope.alloc_native_function(body_used_get_id, None, body_used_get_name, 0)?;
    scope.heap_mut().object_set_prototype(body_used_get, Some(func_proto))?;
    // Root before allocating the property key: `alloc_key` can trigger GC.
    scope.push_root(Value::Object(body_used_get))?;
    let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
    scope.define_property(
      proto,
      body_used_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(body_used_get),
          set: Value::Undefined,
        },
      },
    )?;

    let key = alloc_key(&mut scope, "Request")?;
    scope.define_property(global, key, data_desc(Value::Object(ctor), true))?;
  }

  // --- Response ---
  let response_proto = {
    let call_id = vm.register_native_call(response_ctor_call)?;
    let construct_id = vm.register_native_construct(response_ctor_construct)?;
    let name_s = scope.alloc_string("Response")?;
    scope.push_root(Value::String(name_s))?;
    let ctor = scope.alloc_native_function_with_slots(
      call_id,
      Some(construct_id),
      name_s,
      2,
      &[Value::Number(env_id as f64), Value::Object(headers_proto)],
    )?;
    scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;
    scope.push_root(Value::Object(ctor))?;

    // Prototype methods.
    let Value::Object(proto) = get_data_prop(&mut scope, ctor, "prototype")? else {
      return Err(VmError::InvariantViolation("Response.prototype missing"));
    };
    scope.push_root(Value::Object(proto))?;

    let text_id = vm.register_native_call(response_text_native)?;
    let text_name = scope.alloc_string("text")?;
    scope.push_root(Value::String(text_name))?;
    let text_fn = scope.alloc_native_function(text_id, None, text_name, 0)?;
    scope.heap_mut().object_set_prototype(text_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "text", Value::Object(text_fn), true)?;

    let json_id = vm.register_native_call(response_json_native)?;
    let json_name = scope.alloc_string("json")?;
    scope.push_root(Value::String(json_name))?;
    let json_fn = scope.alloc_native_function(json_id, None, json_name, 0)?;
    scope.heap_mut().object_set_prototype(json_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "json", Value::Object(json_fn), true)?;

    let array_buffer_id = vm.register_native_call(response_array_buffer_native)?;
    let array_buffer_name = scope.alloc_string("arrayBuffer")?;
    scope.push_root(Value::String(array_buffer_name))?;
    let array_buffer_fn = scope.alloc_native_function(array_buffer_id, None, array_buffer_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(array_buffer_fn, Some(func_proto))?;
    set_data_prop(
      &mut scope,
      proto,
      "arrayBuffer",
      Value::Object(array_buffer_fn),
      true,
    )?;

    let blob_id = vm.register_native_call(response_blob_native)?;
    let blob_name = scope.alloc_string("blob")?;
    scope.push_root(Value::String(blob_name))?;
    let blob_fn = scope.alloc_native_function(blob_id, None, blob_name, 0)?;
    scope.heap_mut().object_set_prototype(blob_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "blob", Value::Object(blob_fn), true)?;

    let form_data_id = vm.register_native_call(response_form_data_native)?;
    let form_data_name = scope.alloc_string("formData")?;
    scope.push_root(Value::String(form_data_name))?;
    let form_data_fn = scope.alloc_native_function(form_data_id, None, form_data_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(form_data_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "formData", Value::Object(form_data_fn), true)?;

    let clone_id = vm.register_native_call(response_clone_native)?;
    let clone_name = scope.alloc_string("clone")?;
    scope.push_root(Value::String(clone_name))?;
    let clone_fn = scope.alloc_native_function_with_slots(
      clone_id,
      None,
      clone_name,
      0,
      &[Value::Number(env_id as f64), Value::Object(headers_proto)],
    )?;
    scope.heap_mut().object_set_prototype(clone_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "clone", Value::Object(clone_fn), true)?;

    // bodyUsed accessor (getter only).
    let body_used_get_id = vm.register_native_call(response_body_used_get_native)?;
    let body_used_get_name = scope.alloc_string("get bodyUsed")?;
    scope.push_root(Value::String(body_used_get_name))?;
    let body_used_get = scope.alloc_native_function(body_used_get_id, None, body_used_get_name, 0)?;
    scope.heap_mut().object_set_prototype(body_used_get, Some(func_proto))?;
    // Root before allocating the property key: `alloc_key` can trigger GC.
    scope.push_root(Value::Object(body_used_get))?;
    let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
    scope.define_property(
      proto,
      body_used_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(body_used_get),
          set: Value::Undefined,
        },
      },
    )?;

    // Static methods (`Response.error`, `Response.redirect`).
    let error_id = vm.register_native_call(response_error_native)?;
    let error_name = scope.alloc_string("error")?;
    scope.push_root(Value::String(error_name))?;
    let error_fn = scope.alloc_native_function_with_slots(
      error_id,
      None,
      error_name,
      0,
      &[
        Value::Number(env_id as f64),
        Value::Object(headers_proto),
        Value::Object(proto),
      ],
    )?;
    scope.heap_mut().object_set_prototype(error_fn, Some(func_proto))?;
    set_data_prop(&mut scope, ctor, "error", Value::Object(error_fn), true)?;

    let redirect_id = vm.register_native_call(response_redirect_native)?;
    let redirect_name = scope.alloc_string("redirect")?;
    scope.push_root(Value::String(redirect_name))?;
    let redirect_fn = scope.alloc_native_function_with_slots(
      redirect_id,
      None,
      redirect_name,
      2,
      &[
        Value::Number(env_id as f64),
        Value::Object(headers_proto),
        Value::Object(proto),
      ],
    )?;
    scope.heap_mut().object_set_prototype(redirect_fn, Some(func_proto))?;
    set_data_prop(&mut scope, ctor, "redirect", Value::Object(redirect_fn), true)?;

    let json_id = vm.register_native_call(response_json_static_native)?;
    let json_name = scope.alloc_string("json")?;
    scope.push_root(Value::String(json_name))?;
    let json_fn = scope.alloc_native_function_with_slots(
      json_id,
      None,
      json_name,
      2,
      &[
        Value::Number(env_id as f64),
        Value::Object(headers_proto),
        Value::Object(proto),
      ],
    )?;
    scope.heap_mut().object_set_prototype(json_fn, Some(func_proto))?;
    set_data_prop(&mut scope, ctor, "json", Value::Object(json_fn), true)?;

    let key = alloc_key(&mut scope, "Response")?;
    scope.define_property(global, key, data_desc(Value::Object(ctor), true))?;
    proto
  };

  // --- fetch ---
  {
    let call_id = vm.register_native_call(fetch_call::<Host>)?;
    let name_s = scope.alloc_string("fetch")?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function_with_slots(
      call_id,
      None,
      name_s,
      2,
      &[
        Value::Number(env_id as f64),
        Value::Object(headers_proto),
        Value::Object(response_proto),
      ],
    )?;
    scope.heap_mut().object_set_prototype(func, Some(func_proto))?;
    scope.push_root(Value::Object(func))?;

    let key = alloc_key(&mut scope, "fetch")?;
    scope.define_property(global, key, data_desc(Value::Object(func), true))?;
  }

  // Keep env id visible for debugging.
  let key = alloc_key(&mut scope, ENV_ID_KEY)?;
  scope.define_property(global, key, data_desc(Value::Number(env_id as f64), false))?;

  Ok(bindings)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::clock::VirtualClock;
  use crate::js::event_loop::{EventLoop, RunLimits, RunUntilIdleOutcome};
  use crate::js::realm_module_loader::ModuleLoader;
  use crate::js::JsExecutionOptions;
  use crate::js::window_realm::WindowRealm;
  use crate::js::window_realm::WindowRealmConfig;
  use crate::resource::FetchedResource;
  use std::cell::RefCell;
  use std::collections::VecDeque;
  use std::rc::Rc;
  use std::sync::Arc;
  use std::time::Duration;
  use vm_js::{ExecutionContext, HeapLimits, RootId, VmOptions};
  use vm_js::{Job, RealmId, VmHostHooks};
  use vm_js::PromiseState;
  use webidl_vm_js::{host_from_hooks, WebIdlBindingsHost};

  fn make_user_data(document_url: &str) -> WindowRealmUserData {
    let url = document_url.to_string();
    let module_loader = Rc::new(RefCell::new(ModuleLoader::new(Some(url.clone()))));
    WindowRealmUserData::new(url, module_loader)
  }

  struct DummyHost;

  impl WindowRealmHost for DummyHost {
    fn vm_host_and_window_realm(&mut self) -> (&mut dyn vm_js::VmHost, &mut WindowRealm) {
      panic!("DummyHost.vm_host_and_window_realm should not be called in install tests");
    }
  }

  struct RealmTeardownGuard {
    realm: *mut Realm,
    heap: *mut Heap,
  }

  impl RealmTeardownGuard {
    fn new(realm: &mut Realm, heap: &mut Heap) -> Self {
      Self {
        realm: realm as *mut Realm,
        heap: heap as *mut Heap,
      }
    }
  }

  impl Drop for RealmTeardownGuard {
    fn drop(&mut self) {
      // `vm-js` requires realms to be torn down before drop so persistent roots are cleaned up.
      // Make tests robust to early returns/panics by always tearing down during unwind.
      unsafe {
        (&mut *self.realm).teardown(&mut *self.heap);
      }
    }
  }

  #[derive(Default)]
  struct JobQueueHooks {
    jobs: VecDeque<(Job, Option<RealmId>)>,
  }

  impl VmHostHooks for JobQueueHooks {
    fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
      self.jobs.push_back((job, realm));
    }
  }

  struct TestJobContext<'a, H: vm_js::VmHost> {
    vm: &'a mut Vm,
    heap: &'a mut Heap,
    host: &'a mut H,
    realm: Option<RealmId>,
  }

  impl<H: vm_js::VmHost> vm_js::VmJobContext for TestJobContext<'_, H> {
    fn call(
      &mut self,
      host_hooks: &mut dyn VmHostHooks,
      callee: Value,
      this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      let mut scope = self.heap.scope();
      if let Some(realm) = self.realm {
        let mut vm = self.vm.execution_context_guard(ExecutionContext {
          realm,
          script_or_module: None,
        });
        vm.call_with_host_and_hooks(self.host, &mut scope, host_hooks, callee, this, args)
      } else {
        self
          .vm
          .call_with_host_and_hooks(self.host, &mut scope, host_hooks, callee, this, args)
      }
    }

    fn construct(
      &mut self,
      host_hooks: &mut dyn VmHostHooks,
      callee: Value,
      args: &[Value],
      new_target: Value,
    ) -> Result<Value, VmError> {
      let mut scope = self.heap.scope();
      if let Some(realm) = self.realm {
        let mut vm = self.vm.execution_context_guard(ExecutionContext {
          realm,
          script_or_module: None,
        });
        vm.construct_with_host_and_hooks(self.host, &mut scope, host_hooks, callee, args, new_target)
      } else {
        self.vm.construct_with_host_and_hooks(
          self.host,
          &mut scope,
          host_hooks,
          callee,
          args,
          new_target,
        )
      }
    }

    fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
      self.heap.add_root(value)
    }

    fn remove_root(&mut self, id: RootId) {
      self.heap.remove_root(id);
    }
  }

  fn drain_jobs(
    vm: &mut Vm,
    heap: &mut Heap,
    host: &mut impl vm_js::VmHost,
    hooks: &mut JobQueueHooks,
  ) -> Result<(), VmError> {
    let mut remaining = 1000usize;
    while let Some((job, realm)) = hooks.jobs.pop_front() {
      remaining = remaining
        .checked_sub(1)
        .ok_or(VmError::InvariantViolation("job queue exceeded test limit"))?;
      let mut ctx = TestJobContext {
        vm,
        heap,
        host,
        realm,
      };
      job.run(&mut ctx, hooks)?;
    }
    Ok(())
  }

  #[derive(Default)]
  struct CaptureHostState {
    fulfilled: Option<String>,
    rejected: Option<String>,
  }

  fn capture_promise_string_native(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn vm_js::VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: GcObject,
    _this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let slots = scope.heap().get_function_native_slots(callee)?;
    let kind = slots.get(0).copied().unwrap_or(Value::Number(0.0));
    let kind = number_to_u64(kind).unwrap_or(0);
    let value = args.get(0).copied().unwrap_or(Value::Undefined);
    let s = match value {
      Value::Object(obj) => {
        scope.push_root(Value::Object(obj))?;
        // `vm-js` does not yet implement `ToString` on arbitrary objects; for Error objects (the
        // common Promise rejection case) extract `message` instead so tests can assert on it.
        let message_key_s = scope.alloc_string("message")?;
        scope.push_root(Value::String(message_key_s))?;
        let message_key = PropertyKey::from_string(message_key_s);
        match vm.get(scope, obj, message_key)? {
          Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy().to_string(),
          _ => "[object]".to_string(),
        }
      }
      other => {
        let s = scope.heap_mut().to_string(other)?;
        scope.heap().get_string(s)?.to_utf8_lossy().to_string()
      }
    };
    let state = host
      .as_any_mut()
      .downcast_mut::<CaptureHostState>()
      .ok_or(VmError::InvariantViolation("unexpected host state type"))?;
    if kind == 0 {
      state.fulfilled = Some(s);
    } else {
      state.rejected = Some(s);
    }
    Ok(Value::Undefined)
  }

  #[test]
  fn window_fetch_bindings_drop_unregisters_env() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let env_id = bindings.env_id();
    assert!(envs()
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .contains_key(&env_id));

    drop(bindings);

    assert!(!envs()
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .contains_key(&env_id));
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_body_used_getter_rejects_invalid_this() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let mut scope = heap.scope();

    let callee = scope.alloc_object()?;
    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_body_used_get_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      callee,
      Value::Undefined,
      &[],
    )
    .expect_err("expected illegal invocation TypeError");
    assert!(matches!(err, VmError::TypeError(_)), "err={err}");

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_body_used_getter_rejects_non_response_object() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let mut scope = heap.scope();

    let this_obj = scope.alloc_object()?;
    let callee = scope.alloc_object()?;
    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_body_used_get_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      callee,
      Value::Object(this_obj),
      &[],
    )
    .expect_err("expected illegal invocation TypeError");
    assert!(
      matches!(err, VmError::TypeError("Response: illegal invocation")),
      "err={err}"
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_clone_rejects_non_response_object() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let mut scope = heap.scope();

    let this_obj = scope.alloc_object()?;
    let callee = scope.alloc_object()?;
    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_clone_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      callee,
      Value::Object(this_obj),
      &[],
    )
    .expect_err("expected illegal invocation TypeError");
    assert!(
      matches!(err, VmError::TypeError("Response: illegal invocation")),
      "err={err}"
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn request_ctor_rejects_non_object_init() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    vm.set_user_data(make_user_data("https://example.com/dir/page"));
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
      return Err(VmError::InvariantViolation("Request constructor missing"));
    };

    let url_s = scope.alloc_string("https://example.com")?;
    scope.push_root(Value::String(url_s))?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = request_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      request_ctor,
      &[Value::String(url_s), Value::Number(1.0)],
      Value::Object(request_ctor),
    )
    .expect_err("expected init type error");
    assert!(
      matches!(err, VmError::TypeError("Request init must be an object")),
      "err={err}"
    );

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_ctor_rejects_non_object_init() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      response_ctor,
      &[Value::Undefined, Value::Number(1.0)],
      Value::Object(response_ctor),
    )
    .expect_err("expected init type error");
    assert!(
      matches!(err, VmError::TypeError("Response init must be an object")),
      "err={err}"
    );

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_ctor_rejects_status_out_of_range() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };

    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    set_data_prop(&mut scope, init_obj, "status", Value::Number(199.0), true)?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      response_ctor,
      &[Value::Undefined, Value::Object(init_obj)],
      Value::Object(response_ctor),
    )
    .expect_err("expected status range error");

    let Some(Value::Object(err_obj)) = err.thrown_value() else {
      panic!("expected thrown RangeError object, got {err:?}");
    };
    let name_key = alloc_key(&mut scope, "name")?;
    let name_val = vm.get(&mut scope, err_obj, name_key)?;
    let Value::String(name_s) = name_val else {
      panic!("expected RangeError.name to be a string, got {name_val:?}");
    };
    let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
    assert_eq!(name, "RangeError");

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_ctor_rejects_invalid_status_text() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };

    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    let invalid_status_text = scope.alloc_string("not allowed\n")?;
    set_data_prop(
      &mut scope,
      init_obj,
      "statusText",
      Value::String(invalid_status_text),
      true,
    )?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      response_ctor,
      &[Value::Undefined, Value::Object(init_obj)],
      Value::Object(response_ctor),
    )
    .expect_err("expected invalid statusText error");

    let Some(Value::Object(err_obj)) = err.thrown_value() else {
      panic!("expected thrown TypeError object, got {err:?}");
    };
    let name_key = alloc_key(&mut scope, "name")?;
    let name_val = vm.get(&mut scope, err_obj, name_key)?;
    let Value::String(name_s) = name_val else {
      panic!("expected TypeError.name to be a string, got {name_val:?}");
    };
    let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
    assert_eq!(name, "TypeError");

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_ctor_rejects_body_with_null_body_status() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };

    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    set_data_prop(&mut scope, init_obj, "status", Value::Number(204.0), true)?;

    let body = scope.alloc_string("hello")?;
    scope.push_root(Value::String(body))?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      response_ctor,
      &[Value::String(body), Value::Object(init_obj)],
      Value::Object(response_ctor),
    )
    .expect_err("expected null body status error");

    let Some(Value::Object(err_obj)) = err.thrown_value() else {
      panic!("expected thrown TypeError object, got {err:?}");
    };
    let name_key = alloc_key(&mut scope, "name")?;
    let name_val = vm.get(&mut scope, err_obj, name_key)?;
    let Value::String(name_s) = name_val else {
      panic!("expected TypeError.name to be a string, got {name_val:?}");
    };
    let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
    assert_eq!(name, "TypeError");

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_error_returns_error_response() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };
    let error_key = alloc_key(&mut scope, "error")?;
    let error_fn = vm.get(&mut scope, response_ctor, error_key)?;
    let Value::Object(error_fn) = error_fn else {
      return Err(VmError::InvariantViolation("Response.error missing"));
    };

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let Value::Object(resp_obj) = response_error_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      error_fn,
      Value::Object(response_ctor),
      &[],
    )?
    else {
      return Err(VmError::InvariantViolation("Response.error must return an object"));
    };

    assert!(matches!(
      get_data_prop(&mut scope, resp_obj, "status")?,
      Value::Number(n) if n == 0.0
    ));
    assert!(matches!(get_data_prop(&mut scope, resp_obj, "ok")?, Value::Bool(false)));

    let Value::Object(headers_obj) = get_data_prop(&mut scope, resp_obj, "headers")? else {
      return Err(VmError::InvariantViolation("Response.headers missing"));
    };

    let name = scope.alloc_string("x-test")?;
    scope.push_root(Value::String(name))?;
    let value = scope.alloc_string("a")?;
    scope.push_root(Value::String(value))?;
    let callee = scope.alloc_object()?;
    let err = headers_append_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      callee,
      Value::Object(headers_obj),
      &[Value::String(name), Value::String(value)],
    )
    .expect_err("expected Response.error headers to be immutable");

    let VmError::Throw(Value::Object(err_obj)) = err else {
      panic!("expected thrown TypeError, got {err}");
    };
    let name_key = alloc_key(&mut scope, "name")?;
    let name_val = vm.get(&mut scope, err_obj, name_key)?;
    let Value::String(name_s) = name_val else {
      panic!("expected error.name to be a string, got {name_val:?}");
    };
    let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
    assert_eq!(name, "TypeError");

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_redirect_sets_location_and_immutable_headers() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    // `Response.redirect("relative")` resolves against the current document base URL, which is
    // stored on the VM by `WindowRealm`.
    vm.set_user_data(make_user_data("https://example.com/dir/page"));
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(
        Arc::new(crate::resource::HttpFetcher::new()),
        Some("https://example.com/dir/page".to_string()),
      ),
    )?;

    let mut host_state = ();
    let mut hooks = NoopHooks;

    let result = (|| -> Result<(), VmError> {
      let mut scope = heap.scope();

      let global = realm.global_object();
      let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
        return Err(VmError::InvariantViolation("Response constructor missing"));
      };
      let redirect_key = alloc_key(&mut scope, "redirect")?;
      let redirect_fn = vm.get(&mut scope, response_ctor, redirect_key)?;
      let Value::Object(redirect_fn) = redirect_fn else {
        return Err(VmError::InvariantViolation("Response.redirect missing"));
      };

      let url = scope.alloc_string("foo")?;
      scope.push_root(Value::String(url))?;

      let Value::Object(resp_obj) = response_redirect_native(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        redirect_fn,
        Value::Object(response_ctor),
        &[Value::String(url), Value::Number(302.0)],
      )?
      else {
        return Err(VmError::InvariantViolation("Response.redirect must return an object"));
      };

      assert!(matches!(
        get_data_prop(&mut scope, resp_obj, "status")?,
        Value::Number(n) if n == 302.0
      ));

      let Value::Object(headers_obj) = get_data_prop(&mut scope, resp_obj, "headers")? else {
        return Err(VmError::InvariantViolation("Response.headers missing"));
      };

      let loc_name = scope.alloc_string("Location")?;
      scope.push_root(Value::String(loc_name))?;
      let callee = scope.alloc_object()?;
      let loc_value = headers_get_native(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        callee,
        Value::Object(headers_obj),
        &[Value::String(loc_name)],
      )?;
      let Value::String(loc_s) = loc_value else {
        return Err(VmError::InvariantViolation("Location header missing"));
      };
      let loc = scope.heap().get_string(loc_s)?.to_utf8_lossy();
      assert_eq!(loc, "https://example.com/dir/foo");

      let name = scope.alloc_string("x-test")?;
      scope.push_root(Value::String(name))?;
      let value = scope.alloc_string("a")?;
      scope.push_root(Value::String(value))?;
      let callee = scope.alloc_object()?;
      let err = headers_set_native(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        callee,
        Value::Object(headers_obj),
        &[Value::String(name), Value::String(value)],
      )
      .expect_err("expected Response.redirect headers to be immutable");

      let VmError::Throw(Value::Object(err_obj)) = err else {
        panic!("expected thrown TypeError, got {err}");
      };
      let name_key = alloc_key(&mut scope, "name")?;
      let name_val = vm.get(&mut scope, err_obj, name_key)?;
      let Value::String(name_s) = name_val else {
        panic!("expected error.name to be a string, got {name_val:?}");
      };
      let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
      assert_eq!(name, "TypeError");

      drop(scope);
      Ok(())
    })();

    drop(bindings);
    realm.teardown(&mut heap);
    result
  }

  #[test]
  fn response_redirect_rejects_invalid_status() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(
        Arc::new(crate::resource::HttpFetcher::new()),
        Some("https://example.com/".to_string()),
      ),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };
    let redirect_key = alloc_key(&mut scope, "redirect")?;
    let redirect_fn = vm.get(&mut scope, response_ctor, redirect_key)?;
    let Value::Object(redirect_fn) = redirect_fn else {
      return Err(VmError::InvariantViolation("Response.redirect missing"));
    };

    let url = scope.alloc_string("https://example.com/")?;
    scope.push_root(Value::String(url))?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_redirect_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      redirect_fn,
      Value::Object(response_ctor),
      &[Value::String(url), Value::Number(200.0)],
    )
    .expect_err("expected invalid status RangeError");

    let VmError::Throw(Value::Object(err_obj)) = err else {
      panic!("expected thrown RangeError object, got {err}");
    };
    let name_key = alloc_key(&mut scope, "name")?;
    let name_val = vm.get(&mut scope, err_obj, name_key)?;
    let Value::String(name_s) = name_val else {
      panic!("expected RangeError.name to be a string, got {name_val:?}");
    };
    let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
    assert_eq!(name, "RangeError");

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_redirect_rejects_relative_without_base_url() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };
    let redirect_key = alloc_key(&mut scope, "redirect")?;
    let redirect_fn = vm.get(&mut scope, response_ctor, redirect_key)?;
    let Value::Object(redirect_fn) = redirect_fn else {
      return Err(VmError::InvariantViolation("Response.redirect missing"));
    };

    let url = scope.alloc_string("foo")?;
    scope.push_root(Value::String(url))?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_redirect_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      redirect_fn,
      Value::Object(response_ctor),
      &[Value::String(url), Value::Number(302.0)],
    )
    .expect_err("expected relative without base URL TypeError");

    let VmError::Throw(Value::Object(err_obj)) = err else {
      panic!("expected thrown TypeError object, got {err}");
    };
    let name_key = alloc_key(&mut scope, "name")?;
    let name_val = vm.get(&mut scope, err_obj, name_key)?;
    let Value::String(name_s) = name_val else {
      panic!("expected TypeError.name to be a string, got {name_val:?}");
    };
    let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
    assert_eq!(name, "TypeError");

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_json_sets_body_and_default_content_type() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };
    let json_key = alloc_key(&mut scope, "json")?;
    let json_fn = vm.get(&mut scope, response_ctor, json_key)?;
    let Value::Object(json_fn) = json_fn else {
      return Err(VmError::InvariantViolation("Response.json missing"));
    };

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let Value::Object(resp_obj) = response_json_static_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      json_fn,
      Value::Object(response_ctor),
      &[],
    )?
    else {
      return Err(VmError::InvariantViolation("Response.json must return an object"));
    };

    let (env_id, response_id) = response_info_from_this(&mut scope, Value::Object(resp_obj))?;
    with_env_state(env_id, |state| {
      let res = state
        .responses
        .get(&response_id)
        .ok_or(VmError::InvariantViolation("Response state missing"))?;
      assert_eq!(res.status, 200);
      assert_eq!(
        res.headers.get("content-type").unwrap().as_deref(),
        Some("application/json")
      );
      let body = res.body.as_ref().ok_or(VmError::InvariantViolation(
        "Response.json must create a response body",
      ))?;
      assert_eq!(body.as_bytes(), b"null");
      Ok(())
    })?;

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_json_preserves_existing_content_type() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };
    let json_key = alloc_key(&mut scope, "json")?;
    let json_fn = vm.get(&mut scope, response_ctor, json_key)?;
    let Value::Object(json_fn) = json_fn else {
      return Err(VmError::InvariantViolation("Response.json missing"));
    };

    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    let headers_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(headers_obj))?;
    let existing = scope.alloc_string("text/plain")?;
    scope.push_root(Value::String(existing))?;
    set_data_prop(
      &mut scope,
      headers_obj,
      "Content-Type",
      Value::String(existing),
      true,
    )?;
    set_data_prop(&mut scope, init_obj, "headers", Value::Object(headers_obj), true)?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let Value::Object(resp_obj) = response_json_static_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      json_fn,
      Value::Object(response_ctor),
      &[Value::Number(1.0), Value::Object(init_obj)],
    )?
    else {
      return Err(VmError::InvariantViolation("Response.json must return an object"));
    };

    let (env_id, response_id) = response_info_from_this(&mut scope, Value::Object(resp_obj))?;
    with_env_state(env_id, |state| {
      let res = state
        .responses
        .get(&response_id)
        .ok_or(VmError::InvariantViolation("Response state missing"))?;
      assert_eq!(
        res.headers.get("content-type").unwrap().as_deref(),
        Some("text/plain")
      );
      let body = res.body.as_ref().ok_or(VmError::InvariantViolation(
        "Response.json must create a response body",
      ))?;
      assert_eq!(body.as_bytes(), b"1");
      Ok(())
    })?;

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_json_throws_on_bigint() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };
    let json_key = alloc_key(&mut scope, "json")?;
    let json_fn = vm.get(&mut scope, response_ctor, json_key)?;
    let Value::Object(json_fn) = json_fn else {
      return Err(VmError::InvariantViolation("Response.json missing"));
    };

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = response_json_static_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      json_fn,
      Value::Object(response_ctor),
      &[Value::BigInt(vm_js::JsBigInt::from_u128(1))],
    )
    .expect_err("expected Response.json(BigInt) to throw");

    let Some(Value::Object(err_obj)) = err.thrown_value() else {
      panic!("expected thrown TypeError object, got {err:?}");
    };
    let name_key = alloc_key(&mut scope, "name")?;
    let name_val = vm.get(&mut scope, err_obj, name_key)?;
    let Value::String(name_s) = name_val else {
      panic!("expected TypeError.name to be a string, got {name_val:?}");
    };
    let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
    assert_eq!(name, "TypeError");

    let message_key = alloc_key(&mut scope, "message")?;
    let message_val = vm.get(&mut scope, err_obj, message_key)?;
    let Value::String(message_s) = message_val else {
      panic!("expected TypeError.message to be a string, got {message_val:?}");
    };
    let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
    assert!(
      message.contains("serialize a BigInt"),
      "unexpected error message: {message:?}"
    );

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn fetch_rejects_non_object_init() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(fetch_fn) = get_data_prop(&mut scope, global, "fetch")? else {
      return Err(VmError::InvariantViolation("fetch function missing"));
    };

    let url_s = scope.alloc_string("https://example.com")?;
    scope.push_root(Value::String(url_s))?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = fetch_call::<DummyHost>(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      fetch_fn,
      Value::Undefined,
      &[Value::String(url_s), Value::Number(1.0)],
    )
    .expect_err("expected init type error");
    assert!(
      matches!(err, VmError::TypeError("Request init must be an object")),
      "err={err}"
    );

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn headers_get_rejects_non_headers_object() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let mut scope = heap.scope();

    let this_obj = scope.alloc_object()?;
    let callee = scope.alloc_object()?;
    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = headers_get_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      callee,
      Value::Object(this_obj),
      &[],
    )
    .expect_err("expected illegal invocation TypeError");
    assert!(
      matches!(err, VmError::TypeError("Headers: illegal invocation")),
      "err={err}"
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn headers_get_set_cookie_returns_values_in_order() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(headers_ctor) = get_data_prop(&mut scope, global, "Headers")? else {
      return Err(VmError::InvariantViolation("Headers constructor missing"));
    };

    // new Headers({ "Set-Cookie": "a=1", "Other": "x", "set-cookie": "b=2" })
    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    let a_val = scope.alloc_string("a=1")?;
    scope.push_root(Value::String(a_val))?;
    let x_val = scope.alloc_string("x")?;
    scope.push_root(Value::String(x_val))?;
    let b_val = scope.alloc_string("b=2")?;
    scope.push_root(Value::String(b_val))?;
    set_data_prop(&mut scope, init_obj, "Set-Cookie", Value::String(a_val), true)?;
    set_data_prop(&mut scope, init_obj, "Other", Value::String(x_val), true)?;
    set_data_prop(&mut scope, init_obj, "set-cookie", Value::String(b_val), true)?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let Value::Object(headers_obj) = headers_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      headers_ctor,
      &[Value::Object(init_obj)],
      Value::Object(headers_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation("Headers constructor must return an object"));
    };

    // headers.getSetCookie()
    let get_set_cookie_key = alloc_key(&mut scope, "getSetCookie")?;
    let get_set_cookie_fn = vm.get(&mut scope, headers_obj, get_set_cookie_key)?;
    let Value::Object(get_set_cookie_fn_obj) = get_set_cookie_fn else {
      return Err(VmError::InvariantViolation("Headers.getSetCookie missing"));
    };
    let cookies = vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      Value::Object(get_set_cookie_fn_obj),
      Value::Object(headers_obj),
      &[],
    )?;
    let Value::Object(arr) = cookies else {
      return Err(VmError::InvariantViolation("Headers.getSetCookie must return an object"));
    };

    let len_key = alloc_key(&mut scope, "length")?;
    let len = vm.get(&mut scope, arr, len_key)?;
    assert_eq!(number_to_u64(len)?, 2);

    let idx0 = alloc_key(&mut scope, "0")?;
    let idx1 = alloc_key(&mut scope, "1")?;
    let Value::String(v0_s) = vm.get(&mut scope, arr, idx0)? else {
      return Err(VmError::InvariantViolation("cookies[0] missing"));
    };
    let Value::String(v1_s) = vm.get(&mut scope, arr, idx1)? else {
      return Err(VmError::InvariantViolation("cookies[1] missing"));
    };
    assert_eq!(scope.heap().get_string(v0_s)?.to_utf8_lossy(), "a=1");
    assert_eq!(scope.heap().get_string(v1_s)?.to_utf8_lossy(), "b=2");

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn request_clone_rejects_non_request_object() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let mut scope = heap.scope();

    let this_obj = scope.alloc_object()?;
    let callee = scope.alloc_object()?;
    let mut host_state = ();
    let mut hooks = NoopHooks;
    let err = request_clone_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      callee,
      Value::Object(this_obj),
      &[],
    )
    .expect_err("expected illegal invocation TypeError");
    assert!(
      matches!(err, VmError::TypeError("Request: illegal invocation")),
      "err={err}"
    );

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn request_ctor_rejects_used_body_input_request() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let env_id = bindings.env_id();
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
      return Err(VmError::InvariantViolation("Request constructor missing"));
    };

    let url_s = scope.alloc_string("https://example.com/")?;
    scope.push_root(Value::String(url_s))?;

    // new Request(url, { method: "POST", body: "hello" })
    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    let body_s = scope.alloc_string("hello")?;
    scope.push_root(Value::String(body_s))?;
    let method_s = scope.alloc_string("POST")?;
    scope.push_root(Value::String(method_s))?;
    set_data_prop(&mut scope, init_obj, "method", Value::String(method_s), true)?;
    set_data_prop(&mut scope, init_obj, "body", Value::String(body_s), true)?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let Value::Object(req_obj) = request_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      request_ctor,
      &[Value::String(url_s), Value::Object(init_obj)],
      Value::Object(request_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation("Request constructor must return an object"));
    };

    // Mark the body as used directly in the backing state.
    let request_id = number_to_u64(get_data_prop(&mut scope, req_obj, REQUEST_ID_KEY)?)?;
    with_env_state_mut(env_id, |state| {
      let req = state
        .requests
        .get_mut(&request_id)
        .ok_or(VmError::TypeError("Request: invalid backing request"))?;
      if let Some(body) = req.body.as_mut() {
        let _ = body.consume_bytes().expect("consume_bytes");
      }
      Ok(())
    })?;

    // new Request(existingRequest) should throw if body is already used.
    let err = request_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      request_ctor,
      &[Value::Object(req_obj)],
      Value::Object(request_ctor),
    )
    .expect_err("expected TypeError for used body");

    let Some(Value::Object(err_obj)) = err.thrown_value() else {
      panic!("expected thrown TypeError object, got {err:?}");
    };
    scope.push_root(Value::Object(err_obj))?;
    let name_key = alloc_key(&mut scope, "name")?;
    let msg_key = alloc_key(&mut scope, "message")?;
    let Value::String(name_s) = vm.get(&mut scope, err_obj, name_key)? else {
      return Err(VmError::InvariantViolation("TypeError.name missing"));
    };
    let Value::String(msg_s) = vm.get(&mut scope, err_obj, msg_key)? else {
      return Err(VmError::InvariantViolation("TypeError.message missing"));
    };
    assert_eq!(scope.heap().get_string(name_s)?.to_utf8_lossy(), "TypeError");
    assert_eq!(
      scope.heap().get_string(msg_s)?.to_utf8_lossy(),
      "Request body is already used"
    );

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn fetch_rejects_used_body_input_request() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let env_id = bindings.env_id();
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
      return Err(VmError::InvariantViolation("Request constructor missing"));
    };
    let Value::Object(fetch_fn) = get_data_prop(&mut scope, global, "fetch")? else {
      return Err(VmError::InvariantViolation("fetch function missing"));
    };

    let url_s = scope.alloc_string("https://example.com/")?;
    scope.push_root(Value::String(url_s))?;

    // new Request(url, { method: "POST", body: "hello" })
    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    let body_s = scope.alloc_string("hello")?;
    scope.push_root(Value::String(body_s))?;
    let method_s = scope.alloc_string("POST")?;
    scope.push_root(Value::String(method_s))?;
    set_data_prop(&mut scope, init_obj, "method", Value::String(method_s), true)?;
    set_data_prop(&mut scope, init_obj, "body", Value::String(body_s), true)?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let Value::Object(req_obj) = request_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      request_ctor,
      &[Value::String(url_s), Value::Object(init_obj)],
      Value::Object(request_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation("Request constructor must return an object"));
    };

    let request_id = number_to_u64(get_data_prop(&mut scope, req_obj, REQUEST_ID_KEY)?)?;
    with_env_state_mut(env_id, |state| {
      let req = state
        .requests
        .get_mut(&request_id)
        .ok_or(VmError::TypeError("Request: invalid backing request"))?;
      if let Some(body) = req.body.as_mut() {
        let _ = body.consume_bytes().expect("consume_bytes");
      }
      Ok(())
    })?;

    let err = fetch_call::<DummyHost>(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      fetch_fn,
      Value::Undefined,
      &[Value::Object(req_obj)],
    )
    .expect_err("expected TypeError for used body");

    let Some(Value::Object(err_obj)) = err.thrown_value() else {
      panic!("expected thrown TypeError object, got {err:?}");
    };
    scope.push_root(Value::Object(err_obj))?;
    let name_key = alloc_key(&mut scope, "name")?;
    let msg_key = alloc_key(&mut scope, "message")?;
    let Value::String(name_s) = vm.get(&mut scope, err_obj, name_key)? else {
      return Err(VmError::InvariantViolation("TypeError.name missing"));
    };
    let Value::String(msg_s) = vm.get(&mut scope, err_obj, msg_key)? else {
      return Err(VmError::InvariantViolation("TypeError.message missing"));
    };
    assert_eq!(scope.heap().get_string(name_s)?.to_utf8_lossy(), "TypeError");
    assert_eq!(
      scope.heap().get_string(msg_s)?.to_utf8_lossy(),
      "Request body is already used"
    );

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn request_init_mode_no_cors_applies_header_guard() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
      return Err(VmError::InvariantViolation("Request constructor missing"));
    };

    let url_s = scope.alloc_string("https://example.com/")?;
    scope.push_root(Value::String(url_s))?;

    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;

    let mode_s = scope.alloc_string("no-cors")?;
    set_data_prop(&mut scope, init_obj, "mode", Value::String(mode_s), true)?;

    let headers_init = scope.alloc_object()?;
    scope.push_root(Value::Object(headers_init))?;
    let x_name = scope.alloc_string("x-test")?;
    scope.push_root(Value::String(x_name))?;
    let x_value = scope.alloc_string("a")?;
    set_data_prop(&mut scope, headers_init, "x-test", Value::String(x_value), true)?;
    set_data_prop(&mut scope, init_obj, "headers", Value::Object(headers_init), true)?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let Value::Object(req_obj) = request_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      request_ctor,
      &[Value::String(url_s), Value::Object(init_obj)],
      Value::Object(request_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation("Request constructor must return an object"));
    };

    let Value::String(mode_prop) = get_data_prop(&mut scope, req_obj, "mode")? else {
      return Err(VmError::InvariantViolation("Request.mode missing"));
    };
    let mode = scope.heap().get_string(mode_prop)?.to_utf8_lossy();
    assert_eq!(mode, "no-cors");

    let Value::Object(headers_obj) = get_data_prop(&mut scope, req_obj, "headers")? else {
      return Err(VmError::InvariantViolation("Request.headers missing"));
    };

    let callee = scope.alloc_object()?;
    let value = headers_get_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      callee,
      Value::Object(headers_obj),
      &[Value::String(x_name)],
    )?;
    assert!(matches!(value, Value::Null));

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn request_init_redirect_manual_is_exposed() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
      return Err(VmError::InvariantViolation("Request constructor missing"));
    };

    let url_s = scope.alloc_string("https://example.com/")?;
    scope.push_root(Value::String(url_s))?;

    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    let redirect_s = scope.alloc_string("manual")?;
    set_data_prop(
      &mut scope,
      init_obj,
      "redirect",
      Value::String(redirect_s),
      true,
    )?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let Value::Object(req_obj) = request_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      request_ctor,
      &[Value::String(url_s), Value::Object(init_obj)],
      Value::Object(request_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation("Request constructor must return an object"));
    };

    let Value::String(redirect_prop) = get_data_prop(&mut scope, req_obj, "redirect")? else {
      return Err(VmError::InvariantViolation("Request.redirect missing"));
    };
    let redirect = scope.heap().get_string(redirect_prop)?.to_utf8_lossy();
    assert_eq!(redirect, "manual");

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn request_body_mixin_double_consume_and_clone_preserves_body() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let bindings = match install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    ) {
      Ok(bindings) => bindings,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    };

    let mut host_state = CaptureHostState::default();
    let mut hooks = JobQueueHooks::default();

    let result = (|| -> Result<(), VmError> {
      let mut scope = heap.scope();
      let global = realm.global_object();
      let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
        return Err(VmError::InvariantViolation("Request constructor missing"));
      };

      let url_s = scope.alloc_string("https://example.com/")?;
      scope.push_root(Value::String(url_s))?;

      // new Request(url, { body: "hello" })
      let init_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(init_obj))?;
      let body_s = scope.alloc_string("hello")?;
      scope.push_root(Value::String(body_s))?;
      let method_s = scope.alloc_string("POST")?;
      scope.push_root(Value::String(method_s))?;
      set_data_prop(&mut scope, init_obj, "method", Value::String(method_s), true)?;
      set_data_prop(&mut scope, init_obj, "body", Value::String(body_s), true)?;

      let Value::Object(req_obj) = request_ctor_construct(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        request_ctor,
        &[Value::String(url_s), Value::Object(init_obj)],
        Value::Object(request_ctor),
      )?
      else {
        return Err(VmError::InvariantViolation("Request constructor must return an object"));
      };

      // clone()
      let clone_key = alloc_key(&mut scope, "clone")?;
      let clone_fn = vm.get(&mut scope, req_obj, clone_key)?;
      let cloned = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        clone_fn,
        Value::Object(req_obj),
        &[],
      )?;
      let Value::Object(cloned_obj) = cloned else {
        return Err(VmError::InvariantViolation("Request.clone must return an object"));
      };

      // cloned.text().then(...)
      let text_key = alloc_key(&mut scope, "text")?;
      let text_fn = vm.get(&mut scope, cloned_obj, text_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        text_fn,
        Value::Object(cloned_obj),
        &[],
      )?;

      let capture_id = vm.register_native_call(capture_promise_string_native)?;
      let func_proto = realm.intrinsics().function_prototype();
      let on_fulfilled = {
        let name = scope.alloc_string("onFulfilled")?;
        scope.push_root(Value::String(name))?;
        let f = scope.alloc_native_function_with_slots(
          capture_id,
          None,
          name,
          1,
          &[Value::Number(0.0)],
        )?;
        scope.heap_mut().object_set_prototype(f, Some(func_proto))?;
        f
      };
      let on_rejected = {
        let name = scope.alloc_string("onRejected")?;
        scope.push_root(Value::String(name))?;
        let f = scope.alloc_native_function_with_slots(
          capture_id,
          None,
          name,
          1,
          &[Value::Number(1.0)],
        )?;
        scope.heap_mut().object_set_prototype(f, Some(func_proto))?;
        f
      };

    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation("Request.text must return a Promise object"));
    };
    let then_key = alloc_key(&mut scope, "then")?;
    let then_fn = vm.get(&mut scope, promise_obj, then_key)?;
    vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      then_fn,
      Value::Object(promise_obj),
      &[Value::Object(on_fulfilled), Value::Object(on_rejected)],
    )?;

    // Drain Promise jobs.
    let req_root = scope.heap_mut().add_root(Value::Object(req_obj))?;
    let cloned_root = scope.heap_mut().add_root(Value::Object(cloned_obj))?;
    let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
    let on_rejected_root = scope.heap_mut().add_root(Value::Object(on_rejected))?;
    drop(scope);
    drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;

    assert_eq!(host_state.fulfilled.as_deref(), Some("hello"));
    assert!(host_state.rejected.is_none());

    // Verify `bodyUsed` toggles only on the consumed clone.
    let mut scope = heap.scope();
    let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
    let used_original = vm.get(&mut scope, req_obj, body_used_key)?;
    assert!(matches!(used_original, Value::Bool(false)));
    let used_cloned = vm.get(&mut scope, cloned_obj, body_used_key)?;
    assert!(matches!(used_cloned, Value::Bool(true)));

    // Double-consume should reject.
    host_state.fulfilled = None;
    host_state.rejected = None;

    let text_key = alloc_key(&mut scope, "text")?;
    let text_fn = vm.get(&mut scope, cloned_obj, text_key)?;
    let promise2 = vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      text_fn,
      Value::Object(cloned_obj),
      &[],
    )?;
    let Value::Object(promise2_obj) = promise2 else {
      return Err(VmError::InvariantViolation("Request.text must return a Promise object"));
    };
    let then_key = alloc_key(&mut scope, "then")?;
    let then_fn = vm.get(&mut scope, promise2_obj, then_key)?;
    vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      then_fn,
      Value::Object(promise2_obj),
      &[Value::Object(on_fulfilled), Value::Object(on_rejected)],
    )?;

    drop(scope);
    drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;

    let rejected = host_state.rejected.clone().unwrap_or_default();
    assert!(
      rejected.contains("body is already used"),
      "expected rejection to mention BodyUsed, got {rejected:?}"
    );

    // Smoke test for `json()` consumption.
    let mut scope = heap.scope();
    let url_s = scope.alloc_string("https://example.com/")?;
    scope.push_root(Value::String(url_s))?;
      let init_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(init_obj))?;
      let json_body = scope.alloc_string("null")?;
      let json_method = scope.alloc_string("POST")?;
      scope.push_root(Value::String(json_method))?;
      set_data_prop(&mut scope, init_obj, "method", Value::String(json_method), true)?;
      set_data_prop(&mut scope, init_obj, "body", Value::String(json_body), true)?;
    let Value::Object(req_json_obj) = request_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      request_ctor,
      &[Value::String(url_s), Value::Object(init_obj)],
      Value::Object(request_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation("Request constructor must return an object"));
    };
    host_state.fulfilled = None;
    host_state.rejected = None;
    let json_key = alloc_key(&mut scope, "json")?;
    let json_fn = vm.get(&mut scope, req_json_obj, json_key)?;
    let promise = vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      json_fn,
      Value::Object(req_json_obj),
      &[],
    )?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation("Request.json must return a Promise object"));
    };
    let then_key = alloc_key(&mut scope, "then")?;
    let then_fn = vm.get(&mut scope, promise_obj, then_key)?;
    vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      then_fn,
      Value::Object(promise_obj),
      &[Value::Object(on_fulfilled), Value::Object(on_rejected)],
    )?;

    drop(scope);
    drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
    assert_eq!(host_state.fulfilled.as_deref(), Some("null"));
    assert!(host_state.rejected.is_none());

      heap.remove_root(req_root);
      heap.remove_root(cloned_root);
      heap.remove_root(on_fulfilled_root);
      heap.remove_root(on_rejected_root);

      Ok(())
    })();

    drop(bindings);
    realm.teardown(&mut heap);
    result
  }

  #[test]
  fn request_array_buffer_resolves_to_array_buffer_and_consumes_body() -> Result<(), VmError> {
    #[derive(Default)]
    struct HostState {
      fulfilled_len: Option<u64>,
      rejected: Option<String>,
    }

    fn capture_promise_array_buffer_len_native(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn vm_js::VmHost,
      _hooks: &mut dyn VmHostHooks,
      callee: GcObject,
      _this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      let slots = scope.heap().get_function_native_slots(callee)?;
      let kind = slots.get(0).copied().unwrap_or(Value::Number(0.0));
      let kind = number_to_u64(kind).unwrap_or(0);
      let value = args.get(0).copied().unwrap_or(Value::Undefined);

      let state = host
        .as_any_mut()
        .downcast_mut::<HostState>()
        .ok_or(VmError::InvariantViolation("unexpected host state type"))?;

      if kind == 0 {
        let Value::Object(obj) = value else {
          return Err(VmError::TypeError("expected ArrayBuffer object"));
        };
        if !scope.heap().is_array_buffer_object(obj) {
          return Err(VmError::TypeError("expected ArrayBuffer object"));
        }

        // Use the public `byteLength` getter to validate the resolved object.
        scope.push_root(Value::Object(obj))?;
        let key = alloc_key(scope, "byteLength")?;
        let len_val = vm.get(scope, obj, key)?;
        let len = number_to_u64(len_val)?;
        state.fulfilled_len = Some(len);
      } else {
        // For Promise rejections, extract `message` if the rejection value is an Error object.
        let s = match value {
          Value::Object(obj) => {
            scope.push_root(Value::Object(obj))?;
            let message_key_s = scope.alloc_string("message")?;
            scope.push_root(Value::String(message_key_s))?;
            let message_key = PropertyKey::from_string(message_key_s);
            match vm.get(scope, obj, message_key)? {
              Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy().to_string(),
              _ => "[object]".to_string(),
            }
          }
          other => {
            let s = scope.heap_mut().to_string(other)?;
            scope.heap().get_string(s)?.to_utf8_lossy().to_string()
          }
        };
        state.rejected = Some(s);
      }

      Ok(Value::Undefined)
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let bindings = match install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    ) {
      Ok(bindings) => bindings,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    };

    let mut host_state = HostState::default();
    let mut hooks = JobQueueHooks::default();

    let result = (|| -> Result<(), VmError> {
      let mut scope = heap.scope();
      let global = realm.global_object();
      let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
        return Err(VmError::InvariantViolation("Request constructor missing"));
      };

      let url_s = scope.alloc_string("https://example.com/")?;
      scope.push_root(Value::String(url_s))?;

      let init_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(init_obj))?;
      let body_s = scope.alloc_string("hello")?;
      scope.push_root(Value::String(body_s))?;
      let method_s = scope.alloc_string("POST")?;
      scope.push_root(Value::String(method_s))?;
      set_data_prop(&mut scope, init_obj, "method", Value::String(method_s), true)?;
      set_data_prop(&mut scope, init_obj, "body", Value::String(body_s), true)?;

      let Value::Object(req_obj) = request_ctor_construct(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        request_ctor,
        &[Value::String(url_s), Value::Object(init_obj)],
        Value::Object(request_ctor),
      )?
      else {
        return Err(VmError::InvariantViolation("Request constructor must return an object"));
      };

      let array_buffer_key = alloc_key(&mut scope, "arrayBuffer")?;
      let array_buffer_fn = vm.get(&mut scope, req_obj, array_buffer_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        array_buffer_fn,
        Value::Object(req_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation(
          "Request.arrayBuffer must return a Promise object",
        ));
      };

      let capture_id = vm.register_native_call(capture_promise_array_buffer_len_native)?;
      let func_proto = realm.intrinsics().function_prototype();
      let on_fulfilled = {
        let name = scope.alloc_string("onFulfilled")?;
        scope.push_root(Value::String(name))?;
        let f =
          scope.alloc_native_function_with_slots(capture_id, None, name, 1, &[Value::Number(0.0)])?;
        scope.heap_mut().object_set_prototype(f, Some(func_proto))?;
        f
      };
      let on_rejected = {
        let name = scope.alloc_string("onRejected")?;
        scope.push_root(Value::String(name))?;
        let f =
          scope.alloc_native_function_with_slots(capture_id, None, name, 1, &[Value::Number(1.0)])?;
        scope.heap_mut().object_set_prototype(f, Some(func_proto))?;
        f
      };

      let then_key = alloc_key(&mut scope, "then")?;
      let then_fn = vm.get(&mut scope, promise_obj, then_key)?;
      vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        then_fn,
        Value::Object(promise_obj),
        &[Value::Object(on_fulfilled), Value::Object(on_rejected)],
      )?;

      // Root values needed across microtask execution.
      let req_root = scope.heap_mut().add_root(Value::Object(req_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      let on_rejected_root = scope.heap_mut().add_root(Value::Object(on_rejected))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;

      assert_eq!(host_state.fulfilled_len, Some(5));
      assert!(host_state.rejected.is_none());

      // Verify consumption is observable.
      let mut scope = heap.scope();
      let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
      assert!(matches!(
        vm.get(&mut scope, req_obj, body_used_key)?,
        Value::Bool(true)
      ));

      // Double-consume should reject.
      host_state.fulfilled_len = None;
      host_state.rejected = None;
      let array_buffer_key = alloc_key(&mut scope, "arrayBuffer")?;
      let array_buffer_fn = vm.get(&mut scope, req_obj, array_buffer_key)?;
      let promise2 = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        array_buffer_fn,
        Value::Object(req_obj),
        &[],
      )?;
      let Value::Object(promise2_obj) = promise2 else {
        return Err(VmError::InvariantViolation(
          "Request.arrayBuffer must return a Promise object",
        ));
      };
      let then_key = alloc_key(&mut scope, "then")?;
      let then_fn = vm.get(&mut scope, promise2_obj, then_key)?;
      vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        then_fn,
        Value::Object(promise2_obj),
        &[Value::Object(on_fulfilled), Value::Object(on_rejected)],
      )?;

      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;

      let rejected = host_state.rejected.clone().unwrap_or_default();
      assert!(
        rejected.contains("body is already used"),
        "expected rejection to mention BodyUsed, got {rejected:?}"
      );

      // Cleanup roots.
      heap.remove_root(req_root);
      heap.remove_root(on_fulfilled_root);
      heap.remove_root(on_rejected_root);

      Ok(())
    })();

    drop(bindings);
    realm.teardown(&mut heap);
    result
  }

  #[test]
  fn headers_entries_and_symbol_iterator_are_deterministic() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let mut scope = heap.scope();

    let global = realm.global_object();
    let Value::Object(headers_ctor) = get_data_prop(&mut scope, global, "Headers")? else {
      return Err(VmError::InvariantViolation("Headers constructor missing"));
    };

    // new Headers({ b: "2", a: "1" })
    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    let b_val = scope.alloc_string("2")?;
    let a_val = scope.alloc_string("1")?;
    set_data_prop(&mut scope, init_obj, "b", Value::String(b_val), true)?;
    set_data_prop(&mut scope, init_obj, "a", Value::String(a_val), true)?;

    let mut host_state = ();
    let mut hooks = NoopHooks;
    let Value::Object(headers_obj) = headers_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      headers_ctor,
      &[Value::Object(init_obj)],
      Value::Object(headers_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation("Headers constructor must return an object"));
    };

    let entries_key = alloc_key(&mut scope, "entries")?;
    let entries_fn = vm.get(&mut scope, headers_obj, entries_key)?;
    let Value::Object(entries_fn_obj) = entries_fn else {
      return Err(VmError::InvariantViolation("Headers.entries missing"));
    };

    let iter = vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      Value::Object(entries_fn_obj),
      Value::Object(headers_obj),
      &[],
    )?;
    let Value::Object(iter_obj) = iter else {
      return Err(VmError::InvariantViolation("Headers.entries must return an object"));
    };

    let next_key = alloc_key(&mut scope, "next")?;
    let next_fn = vm.get(&mut scope, iter_obj, next_key)?;
    let Value::Object(next_fn_obj) = next_fn else {
      return Err(VmError::InvariantViolation("Headers iterator next missing"));
    };

    // First next(): ["a", "1"]
    let Value::Object(res1_obj) = vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      Value::Object(next_fn_obj),
      Value::Object(iter_obj),
      &[],
    )?
    else {
      return Err(VmError::InvariantViolation("Iterator result must be an object"));
    };
    let done_key = alloc_key(&mut scope, "done")?;
    let value_key = alloc_key(&mut scope, "value")?;
    assert!(matches!(vm.get(&mut scope, res1_obj, done_key)?, Value::Bool(false)));
    let Value::Object(pair1) = vm.get(&mut scope, res1_obj, value_key)? else {
      return Err(VmError::InvariantViolation("Iterator value must be an object"));
    };
    let k0 = alloc_key(&mut scope, "0")?;
    let k1 = alloc_key(&mut scope, "1")?;
    let Value::String(k1_s) = vm.get(&mut scope, pair1, k0)? else {
      return Err(VmError::InvariantViolation("pair[0] missing"));
    };
    let Value::String(v1_s) = vm.get(&mut scope, pair1, k1)? else {
      return Err(VmError::InvariantViolation("pair[1] missing"));
    };
    assert_eq!(scope.heap().get_string(k1_s)?.to_utf8_lossy(), "a");
    assert_eq!(scope.heap().get_string(v1_s)?.to_utf8_lossy(), "1");

    // Second next(): ["b", "2"]
    let Value::Object(res2_obj) = vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      Value::Object(next_fn_obj),
      Value::Object(iter_obj),
      &[],
    )?
    else {
      return Err(VmError::InvariantViolation("Iterator result must be an object"));
    };
    assert!(matches!(vm.get(&mut scope, res2_obj, done_key)?, Value::Bool(false)));
    let Value::Object(pair2) = vm.get(&mut scope, res2_obj, value_key)? else {
      return Err(VmError::InvariantViolation("Iterator value must be an object"));
    };
    let Value::String(k2_s) = vm.get(&mut scope, pair2, k0)? else {
      return Err(VmError::InvariantViolation("pair[0] missing"));
    };
    let Value::String(v2_s) = vm.get(&mut scope, pair2, k1)? else {
      return Err(VmError::InvariantViolation("pair[1] missing"));
    };
    assert_eq!(scope.heap().get_string(k2_s)?.to_utf8_lossy(), "b");
    assert_eq!(scope.heap().get_string(v2_s)?.to_utf8_lossy(), "2");

    // [Symbol.iterator] should alias entries().
    let sym_key = alloc_symbol_key(&mut scope, "Symbol.iterator")?;
    let sym_fn = vm.get(&mut scope, headers_obj, sym_key)?;
    let Value::Object(sym_fn_obj) = sym_fn else {
      return Err(VmError::InvariantViolation("Headers @@iterator missing"));
    };
    assert_eq!(sym_fn_obj, entries_fn_obj);

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_type_and_redirected_match_core_response() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let env_id = bindings.env_id();

    let response_id = with_env_state_mut(env_id, |state| {
      let id = state.alloc_id();
      let mut response = CoreResponse::new(200);
      response.r#type = ResponseType::Cors;
      response.redirected = true;
      response.url = "https://example.com/".to_string();
      response.status_text = "OK".to_string();
      state.responses.insert(id, response);
      Ok(id)
    })?;

    let mut scope = heap.scope();
    let global = realm.global_object();
    let Value::Object(headers_ctor) = get_data_prop(&mut scope, global, "Headers")? else {
      return Err(VmError::InvariantViolation("Headers constructor missing"));
    };
    let Value::Object(headers_proto) = get_data_prop(&mut scope, headers_ctor, "prototype")? else {
      return Err(VmError::InvariantViolation("Headers.prototype missing"));
    };
    let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
      return Err(VmError::InvariantViolation("Response constructor missing"));
    };
    let Value::Object(response_proto) = get_data_prop(&mut scope, response_ctor, "prototype")? else {
      return Err(VmError::InvariantViolation("Response.prototype missing"));
    };

    let resp_obj = make_response_wrapper(&mut scope, env_id, headers_proto, response_proto, response_id)?;
    let Value::String(type_s) = get_data_prop(&mut scope, resp_obj, "type")? else {
      return Err(VmError::InvariantViolation("Response.type missing"));
    };
    let ty = scope.heap().get_string(type_s)?.to_utf8_lossy();
    assert_eq!(ty, "cors");
    assert!(matches!(get_data_prop(&mut scope, resp_obj, "redirected")?, Value::Bool(true)));

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_blob_resolves_to_blob_and_uses_content_type_essence() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host.window.exec_script(
      "new Response('hi', { headers: { 'Content-Type': 'Text/Plain; charset=utf-8' } }).blob()",
    )?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Response.blob must return a Promise object",
      ));
    };
    assert_eq!(
      host.window.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = host.window.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Response.blob promise missing result",
      ));
    };
    let Value::Object(blob_obj) = result else {
      return Err(VmError::InvariantViolation(
        "Response.blob must resolve to a Blob object",
      ));
    };

    let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(blob_obj))?;

    let size_key = alloc_key(&mut scope, "size")?;
    assert_eq!(vm.get(&mut scope, blob_obj, size_key)?, Value::Number(2.0));

    let type_key = alloc_key(&mut scope, "type")?;
    let Value::String(type_s) = vm.get(&mut scope, blob_obj, type_key)? else {
      return Err(VmError::InvariantViolation("Blob.type missing"));
    };
    assert_eq!(scope.heap().get_string(type_s)?.to_utf8_lossy(), "text/plain");

    Ok(())
  }

  #[test]
  fn request_blob_resolves_to_blob_and_uses_content_type_essence() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host.window.exec_script(
      "new Request('https://example.invalid/', { method: 'POST', body: 'hi', headers: { 'Content-Type': 'Text/Plain; charset=utf-8' } }).blob()",
    )?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Request.blob must return a Promise object",
      ));
    };
    assert_eq!(
      host.window.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = host.window.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Request.blob promise missing result",
      ));
    };
    let Value::Object(blob_obj) = result else {
      return Err(VmError::InvariantViolation(
        "Request.blob must resolve to a Blob object",
      ));
    };

    let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(blob_obj))?;

    let size_key = alloc_key(&mut scope, "size")?;
    assert_eq!(vm.get(&mut scope, blob_obj, size_key)?, Value::Number(2.0));

    let type_key = alloc_key(&mut scope, "type")?;
    let Value::String(type_s) = vm.get(&mut scope, blob_obj, type_key)? else {
      return Err(VmError::InvariantViolation("Blob.type missing"));
    };
    assert_eq!(scope.heap().get_string(type_s)?.to_utf8_lossy(), "text/plain");

    Ok(())
  }

  #[test]
  fn response_ctor_accepts_uint8_array_body() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host.window.exec_script(
      "(function(){\
         const bytes = new Uint8Array(2);\
         bytes[0] = 65;\
         bytes[1] = 66;\
         return new Response(bytes).arrayBuffer();\
       })()",
    )?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Response.arrayBuffer must return a Promise object",
      ));
    };
    assert_eq!(
      host.window.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = host.window.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Response.arrayBuffer promise missing result",
      ));
    };
    let Value::Object(ab_obj) = result else {
      return Err(VmError::InvariantViolation(
        "Response.arrayBuffer must resolve to an ArrayBuffer object",
      ));
    };

    let (_vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(ab_obj))?;
    assert!(scope.heap().is_array_buffer_object(ab_obj));
    assert_eq!(scope.heap().array_buffer_data(ab_obj)?, &[65, 66]);

    Ok(())
  }

  #[test]
  fn response_ctor_accepts_url_search_params_body_and_sets_content_type() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result_obj = host.window.exec_script(
      "(function(){\
         const resp = new Response(new URLSearchParams('a=1&b=2'));\
         return { ct: resp.headers.get('Content-Type'), promise: resp.text() };\
       })()",
    )?;
    let Value::Object(result_obj) = result_obj else {
      return Err(VmError::InvariantViolation(
        "expected response ctor test script to return an object",
      ));
    };

    let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(result_obj))?;

    let ct_key = alloc_key(&mut scope, "ct")?;
    let Value::String(ct_s) = vm.get(&mut scope, result_obj, ct_key)? else {
      return Err(VmError::InvariantViolation("expected ct to be a string"));
    };
    assert_eq!(
      scope.heap().get_string(ct_s)?.to_utf8_lossy(),
      "application/x-www-form-urlencoded;charset=UTF-8"
    );

    let promise_key = alloc_key(&mut scope, "promise")?;
    let promise = vm.get(&mut scope, result_obj, promise_key)?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation("expected promise to be an object"));
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = scope.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation("Response.text promise missing result"));
    };
    let Value::String(text_s) = result else {
      return Err(VmError::InvariantViolation(
        "Response.text must resolve to a string",
      ));
    };
    assert_eq!(scope.heap().get_string(text_s)?.to_utf8_lossy(), "a=1&b=2");

    Ok(())
  }

  #[test]
  fn response_ctor_accepts_blob_body_and_sets_content_type() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result_obj = host.window.exec_script(
      "(function(){\
         const resp = new Response(new Blob(['hi'], { type: 'Text/Plain' }));\
         return { ct: resp.headers.get('Content-Type'), promise: resp.text() };\
       })()",
    )?;
    let Value::Object(result_obj) = result_obj else {
      return Err(VmError::InvariantViolation(
        "expected response ctor test script to return an object",
      ));
    };

    let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(result_obj))?;

    let ct_key = alloc_key(&mut scope, "ct")?;
    let Value::String(ct_s) = vm.get(&mut scope, result_obj, ct_key)? else {
      return Err(VmError::InvariantViolation("expected ct to be a string"));
    };
    assert_eq!(scope.heap().get_string(ct_s)?.to_utf8_lossy(), "text/plain");

    let promise_key = alloc_key(&mut scope, "promise")?;
    let promise = vm.get(&mut scope, result_obj, promise_key)?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation("expected promise to be an object"));
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = scope.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation("Response.text promise missing result"));
    };
    let Value::String(text_s) = result else {
      return Err(VmError::InvariantViolation(
        "Response.text must resolve to a string",
      ));
    };
    assert_eq!(scope.heap().get_string(text_s)?.to_utf8_lossy(), "hi");

    Ok(())
  }

  #[test]
  fn response_ctor_accepts_form_data_body_and_sets_boundary() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result_obj = host.window.exec_script(
      r#"(function(){
           const fd = new FormData();
           fd.append('a', 'b');
           fd.append('file', new Blob(['hi'], { type: 'text/plain' }), 'f.txt');
           const resp = new Response(fd);
           return { ct: resp.headers.get('Content-Type'), promise: resp.text() };
         })()"#,
    )?;
    let Value::Object(result_obj) = result_obj else {
      return Err(VmError::InvariantViolation(
        "expected response ctor test script to return an object",
      ));
    };

    let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(result_obj))?;

    let ct_key = alloc_key(&mut scope, "ct")?;
    let Value::String(ct_s) = vm.get(&mut scope, result_obj, ct_key)? else {
      return Err(VmError::InvariantViolation("expected ct to be a string"));
    };
    assert_eq!(
      scope.heap().get_string(ct_s)?.to_utf8_lossy(),
      "multipart/form-data; boundary=----fastrenderformdata1"
    );

    let promise_key = alloc_key(&mut scope, "promise")?;
    let promise = vm.get(&mut scope, result_obj, promise_key)?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation("expected promise to be an object"));
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = scope.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation("Response.text promise missing result"));
    };
    let Value::String(text_s) = result else {
      return Err(VmError::InvariantViolation(
        "Response.text must resolve to a string",
      ));
    };
    let text = scope.heap().get_string(text_s)?.to_utf8_lossy();

    let expected = concat!(
      "------fastrenderformdata1\r\n",
      "Content-Disposition: form-data; name=\"a\"\r\n",
      "\r\n",
      "b\r\n",
      "------fastrenderformdata1\r\n",
      "Content-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n",
      "Content-Type: text/plain\r\n",
      "\r\n",
      "hi\r\n",
      "------fastrenderformdata1--\r\n"
    );
    assert_eq!(text, expected);

    Ok(())
  }

  fn clone_form_data_entries_for_test(
    host: &mut EventLoopHost,
    fd_obj: GcObject,
  ) -> Result<Vec<window_form_data::FormDataEntry>, VmError> {
    let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
    let vm_guard = vm.execution_context_guard(ExecutionContext {
      realm: realm.id(),
      script_or_module: None,
    });
    window_form_data::clone_form_data_entries_for_fetch(&vm_guard, heap, Value::Object(fd_obj))?
      .ok_or(VmError::InvariantViolation("expected FormData object"))
  }

  #[test]
  fn request_form_data_parses_url_search_params_body() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host.window.exec_script(
      "new Request('https://example.invalid/', { method: 'POST', body: new URLSearchParams('a=1&b=2') }).formData()",
    )?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Request.formData must return a Promise object",
      ));
    };

    let fd_obj = {
      let heap = host.window.heap();
      assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
      let Some(result) = heap.promise_result(promise_obj)? else {
        return Err(VmError::InvariantViolation(
          "Request.formData promise missing result",
        ));
      };
      let Value::Object(fd_obj) = result else {
        return Err(VmError::InvariantViolation(
          "Request.formData must resolve to a FormData object",
        ));
      };
      fd_obj
    };

    let entries = clone_form_data_entries_for_test(&mut host, fd_obj)?;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "a");
    match &entries[0].value {
      window_form_data::FormDataValue::String(value) => assert_eq!(value, "1"),
      other => panic!("expected string entry, got {other:?}"),
    }
    assert_eq!(entries[1].name, "b");
    match &entries[1].value {
      window_form_data::FormDataValue::String(value) => assert_eq!(value, "2"),
      other => panic!("expected string entry, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn response_form_data_parses_multipart_body_roundtrip() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host.window.exec_script(
      r#"(function(){
           const fd = new FormData();
           fd.append('a', 'b');
           fd.append('file', new Blob(['hi'], { type: 'text/plain' }), 'f.txt');
           return new Response(fd).formData();
         })()"#,
    )?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Response.formData must return a Promise object",
      ));
    };

    let fd_obj = {
      let heap = host.window.heap();
      assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
      let Some(result) = heap.promise_result(promise_obj)? else {
        return Err(VmError::InvariantViolation(
          "Response.formData promise missing result",
        ));
      };
      let Value::Object(fd_obj) = result else {
        return Err(VmError::InvariantViolation(
          "Response.formData must resolve to a FormData object",
        ));
      };
      fd_obj
    };

    let entries = clone_form_data_entries_for_test(&mut host, fd_obj)?;
    assert_eq!(entries.len(), 2);

    assert_eq!(entries[0].name, "a");
    match &entries[0].value {
      window_form_data::FormDataValue::String(value) => assert_eq!(value, "b"),
      other => panic!("expected string entry, got {other:?}"),
    }

    assert_eq!(entries[1].name, "file");
    match &entries[1].value {
      window_form_data::FormDataValue::Blob { data, filename } => {
        assert_eq!(filename, "f.txt");
        assert_eq!(data.r#type, "text/plain");
        assert_eq!(data.bytes.as_slice(), b"hi");
      }
      other => panic!("expected blob entry, got {other:?}"),
    }

    Ok(())
  }

  struct EventLoopHost {
    host_ctx: (),
    window: WindowRealm,
  }

  impl EventLoopHost {
    fn new_with_js_execution_options(js_execution_options: JsExecutionOptions) -> Self {
      let window = WindowRealm::new_with_js_execution_options(
        WindowRealmConfig::new("https://example.invalid/"),
        js_execution_options,
      )
      .unwrap();
      Self { host_ctx: (), window }
    }
  }

  impl WindowRealmHost for EventLoopHost {
    fn vm_host_and_window_realm(&mut self) -> (&mut dyn vm_js::VmHost, &mut WindowRealm) {
      let EventLoopHost { host_ctx, window } = self;
      (host_ctx, window)
    }
  }

  struct StaticOkFetcher;

  impl ResourceFetcher for StaticOkFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Ok(FetchedResource::new(
        format!("ok:{url}").into_bytes(),
        Some("text/plain".to_string()),
      ))
    }
  }

  #[derive(Debug, Clone)]
  struct CapturedHttpRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
  }

  #[derive(Default)]
  struct CaptureHttpRequestFetcher {
    last: std::sync::Mutex<Option<CapturedHttpRequest>>,
  }

  impl CaptureHttpRequestFetcher {
    fn take(&self) -> Option<CapturedHttpRequest> {
      self.last.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
  }

  impl ResourceFetcher for CaptureHttpRequestFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Ok(FetchedResource::new(
        format!("ok:{url}").into_bytes(),
        Some("text/plain".to_string()),
      ))
    }

    fn fetch_http_request(&self, req: crate::resource::HttpRequest<'_>) -> crate::error::Result<FetchedResource> {
      let captured = CapturedHttpRequest {
        method: req.method.to_string(),
        url: req.fetch.url.to_string(),
        headers: req.headers.to_vec(),
        body: req.body.map(|b| b.to_vec()),
      };
      *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(captured);
      Ok(FetchedResource::new(
        format!("ok:{}", req.fetch.url).into_bytes(),
        Some("text/plain".to_string()),
      ))
    }
  }

  fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case(name))
      .map(|(_, v)| v.as_str())
  }

  #[test]
  fn fetch_blob_body_sends_bytes_and_sets_content_type() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(fetcher.clone(), Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host);
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window.exec_script_with_host_and_hooks(
        host_ctx,
        &mut hooks,
        "fetch('https://example.invalid/upload', { method: 'POST', body: new Blob(['hi'], { type: 'text/plain' }) });",
      ).unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let captured = fetcher.take().expect("expected fetch_http_request call");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.url, "https://example.invalid/upload");
    assert_eq!(captured.body.as_deref(), Some(b"hi".as_slice()));
    assert_eq!(
      header_value(&captured.headers, "content-type"),
      Some("text/plain"),
      "headers={:?}",
      captured.headers
    );

    Ok(())
  }

  #[test]
  fn fetch_url_search_params_body_sets_content_type_and_serializes() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(fetcher.clone(), Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host);
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window.exec_script_with_host_and_hooks(
        host_ctx,
        &mut hooks,
        "fetch('https://example.invalid/submit', { method: 'POST', body: new URLSearchParams('a=1&b=2') });",
      ).unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let captured = fetcher.take().expect("expected fetch_http_request call");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.url, "https://example.invalid/submit");
    assert_eq!(captured.body.as_deref(), Some(b"a=1&b=2".as_slice()));
    assert_eq!(
      header_value(&captured.headers, "content-type"),
      Some("application/x-www-form-urlencoded;charset=UTF-8"),
      "headers={:?}",
      captured.headers
    );

    Ok(())
  }

  #[test]
  fn fetch_form_data_body_encodes_multipart_and_sets_boundary() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(fetcher.clone(), Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host);
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window.exec_script_with_host_and_hooks(
        host_ctx,
        &mut hooks,
        r#"
            const fd = new FormData();
            fd.append('a', 'b');
            fd.append('file', new Blob(['hi'], { type: 'text/plain' }), 'f.txt');
            fetch('https://example.invalid/multipart', { method: 'POST', body: fd });
          "#,
      ).unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let captured = fetcher.take().expect("expected fetch_http_request call");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.url, "https://example.invalid/multipart");

    let ct = header_value(&captured.headers, "content-type").expect("Content-Type should be set");
    assert_eq!(ct, "multipart/form-data; boundary=----fastrenderformdata1");

    let expected = concat!(
      "------fastrenderformdata1\r\n",
      "Content-Disposition: form-data; name=\"a\"\r\n",
      "\r\n",
      "b\r\n",
      "------fastrenderformdata1\r\n",
      "Content-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\n",
      "Content-Type: text/plain\r\n",
      "\r\n",
      "hi\r\n",
      "------fastrenderformdata1--\r\n"
    );
    assert_eq!(
      captured.body.as_deref(),
      Some(expected.as_bytes()),
      "body={:?}",
      captured.body.as_deref()
    );

    Ok(())
  }

  #[test]
  fn fetch_completion_microtask_respects_max_instruction_count() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);

    let mut opts = JsExecutionOptions::default();
    opts.max_instruction_count = Some(0);
    // Keep wall-time generous so we deterministically hit OutOfFuel first.
    opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));

    let mut host = EventLoopHost::new_with_js_execution_options(opts);

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };
 
    // Create the fetch promise under an explicit unlimited VM budget so we can enqueue work even
    // when the realm's JsExecutionOptions fuel limit is 0 (the test case).
    let promise_root: RootId = (|| -> crate::error::Result<RootId> {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host);
      hooks.set_event_loop(&mut event_loop);

      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let prev_budget = vm.swap_budget_state(vm_js::Budget::unlimited(100));
      vm.tick().expect("tick under unlimited budget");

      let root: RootId = {
        let mut scope = heap.scope();
        let global = realm.global_object();
        scope
          .push_root(Value::Object(global))
          .expect("push root global");

        let fetch_key = alloc_key(&mut scope, "fetch").expect("alloc fetch key");
        let fetch = vm
          .get(&mut scope, global, fetch_key)
          .expect("globalThis.fetch should be defined");

        let url_s = scope
          .alloc_string("https://example.invalid/ok")
          .expect("alloc url string");
        scope
          .push_root(Value::String(url_s))
          .expect("push root url string");

        let promise = vm
          .call_with_host_and_hooks(
            &mut host.host_ctx,
            &mut scope,
            &mut hooks,
            fetch,
            Value::Undefined,
            &[Value::String(url_s)],
          )
          .expect("fetch() should return a promise");

        scope
          .heap_mut()
          .add_root(promise)
          .expect("root fetch promise")
      };

      vm.restore_budget_state(prev_budget);
      if let Some(err) = hooks.finish(heap) {
        return Err(err);
      }
      Ok(root)
    })()?;

    let err = event_loop
      .run_until_idle(&mut host, RunLimits::unbounded())
      .expect_err("expected fetch completion microtask to terminate due to fuel=0");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
      msg.contains("out of fuel"),
      "expected OutOfFuel termination, got: {msg}"
    );

    let promise_state = {
      let (_vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
      let promise_value = heap.get_root(promise_root).unwrap_or(Value::Undefined);
      let Value::Object(promise_obj) = promise_value else {
        panic!("expected fetch promise object");
      };
      heap.promise_state(promise_obj).expect("promise_state")
    };
    assert_eq!(
      promise_state,
      PromiseState::Pending,
      "fetch promise should remain pending when completion microtask is out-of-fuel"
    );

    host.window.heap_mut().remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn webidl_host_slot_available_in_fetch_completion_thenable_assimilation() -> crate::error::Result<()> {
    #[derive(Default)]
    struct DispatchBindingsHost {
      calls: usize,
    }

    impl WebIdlBindingsHost for DispatchBindingsHost {
      fn call_operation(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _receiver: Option<Value>,
        _interface: &'static str,
        _operation: &'static str,
        _overload: usize,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        self.calls += 1;
        Ok(Value::Undefined)
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
          "constructor dispatch not implemented in DispatchBindingsHost",
        ))
      }
    }

    struct DispatchEventLoopHost {
      host_ctx: (),
      bindings_host: DispatchBindingsHost,
      window: WindowRealm,
    }

    impl DispatchEventLoopHost {
      fn new() -> Self {
        let window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
        Self {
          host_ctx: (),
          bindings_host: DispatchBindingsHost::default(),
          window,
        }
      }
    }

    impl WindowRealmHost for DispatchEventLoopHost {
      fn vm_host_and_window_realm(&mut self) -> (&mut dyn vm_js::VmHost, &mut WindowRealm) {
        let DispatchEventLoopHost { host_ctx, window, .. } = self;
        (host_ctx, window)
      }

      fn webidl_bindings_host(&mut self) -> Option<&mut dyn WebIdlBindingsHost> {
        Some(&mut self.bindings_host)
      }
    }

    fn native_webidl_dispatch(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      _host: &mut dyn vm_js::VmHost,
      hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      let host = host_from_hooks(hooks)?;
      let _ = host.call_operation(vm, scope, None, "TestInterface", "testOp", 0, &[])?;
      Ok(Value::Undefined)
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<DispatchEventLoopHost>::with_clock(clock);
    let mut host = DispatchEventLoopHost::new();

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DispatchEventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let call_id = vm.register_native_call(native_webidl_dispatch).unwrap();
      let mut scope = heap.scope();
      let global = realm.global_object();
      scope.push_root(Value::Object(global)).expect("push root global");

      let name_s = scope.alloc_string("__webidl_dispatch").unwrap();
      scope.push_root(Value::String(name_s)).unwrap();
      let func = scope.alloc_native_function(call_id, None, name_s, 0).unwrap();
      scope
        .heap_mut()
        .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))
        .unwrap();
      scope.push_root(Value::Object(func)).unwrap();

      let key = alloc_key(&mut scope, "__webidl_dispatch").unwrap();
      scope
        .define_property(global, key, data_desc(Value::Object(func), true))
        .unwrap();
    }
 
    // Make `Response` objects thenable so resolving the fetch promise triggers thenable
    // assimilation, which calls user code during the fetch completion microtask.
    {
      let mut hooks = VmJsEventLoopHooks::<DispatchEventLoopHost>::new_with_host(&mut host);
      hooks.set_event_loop(&mut event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm();
      let result = window_realm.exec_script_with_host_and_hooks(
        vm_host,
        &mut hooks,
        "Response.prototype.then = function(resolve, _reject) {\n\
           globalThis.__webidl_dispatch();\n\
           resolve(1);\n\
         };\n\
         fetch('https://example.invalid/ok');",
      );
      if let Some(err) = hooks.finish(window_realm.heap_mut()) {
        return Err(err);
      }
      result.map_err(|e| crate::error::Error::Other(e.to_string()))?;
    }

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.bindings_host.calls, 1);
    Ok(())
  }
}
