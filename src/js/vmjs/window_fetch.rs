//! Minimal WHATWG Fetch bindings (`fetch`/`Headers`/`Request`/`Response`) for the `vm-js` Window realm.
//!
//! This module is a deliberately small, test-focused wrapper around the core Fetch
//! implementation in [`crate::resource::web_fetch`]. It is **not** a complete browser Fetch stack.
//!
//! ## `fetch(input, init)` / `new Request(input, init)`
//!
//! Implemented `RequestInit` fields:
//! - `method`
//! - `headers`
//! - `body` (`string`, `ArrayBuffer`, `Uint8Array`, `URLSearchParams`, `Blob`, `FormData`, `ReadableStream`)
//! - `mode` (`navigate`/`same-origin`/`no-cors`/`cors`)
//! - `redirect` (`follow`/`error`/`manual`)
//! - `referrer`
//! - `referrerPolicy`
//! - `credentials` (`omit`/`same-origin`/`include`)
//! - `signal`
//!
//! Unimplemented/ignored fields (non-exhaustive): `cache`, `integrity`, `priority`, `keepalive`,
//! `duplex`, and others.
//!
//! ### Request body streaming
//!
//! The Rust core fetch layer only accepts in-memory request bodies. If a `ReadableStream` is
//! provided as `init.body`, the stream is fully buffered into memory (bounded by
//! [`WebFetchLimits`]) before the request is dispatched; true streaming uploads are not supported.
//!
//! ## Responses
//!
//! `Response.body` is exposed as a `ReadableStream`, and the `Response` constructor also accepts
//! `ReadableStream` bodies (byte streams).
//!
//! ### AbortSignal semantics
//!
//! `signal` is observed in a best-effort way:
//! - If already aborted, `fetch()` rejects immediately with `signal.reason`.
//! - If aborted after `fetch()` returns, the returned promise rejects promptly.
//!
//! Spec deviation: aborting cannot cancel an in-flight request because the underlying
//! [`ResourceFetcher`] API is synchronous; the work still completes on the Rust side, but the JS
//! promise rejects instead of resolving.

use crate::js::event_loop::TaskSource;
use crate::js::time;
use crate::js::url_resolve::{resolve_url, UrlResolveError};
use crate::js::window_blob;
use crate::js::window_form_data;
use crate::js::window_object_url;
use crate::js::window_realm::{WindowRealmHost, WindowRealmUserData};
use crate::js::window_streams;
use crate::js::window_timers::{
  event_loop_mut_from_hooks, vm_error_to_event_loop_error, VmJsEventLoopHooks,
};
use crate::js::window_url;
use crate::resource::web_fetch::{
  execute_web_fetch, Body, Headers as CoreHeaders, HeadersGuard, Request as CoreRequest,
  RequestCredentials, RequestMode, RequestRedirect, Response as CoreResponse, ResponseType,
  WebFetchError, WebFetchExecutionContext, WebFetchLimits,
};
use crate::resource::{
  origin_from_url, DocumentOrigin, FetchDestination, ReferrerPolicy, ResourceFetcher,
};
use http::Method;
use std::char::decode_utf16;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use vm_js::{
  GcObject, Heap, HostSlots, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
  RealmId, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, WeakGcObject,
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

// Hidden per-instance properties for stream wrappers.
const REQUEST_BODY_STREAM_KEY: &str = "__fastrender_request_body_stream";
const RESPONSE_BODY_STREAM_KEY: &str = "__fastrender_response_body_stream";

// Hidden per-stream properties for external (user-provided) ReadableStream bodies.
//
// Note: Fetch's `bodyUsed` is derived from the ReadableStream disturbed state, which becomes true
// after the first read/cancel. Since VM-JS ReadableStreams do not currently expose disturbed, track
// it on the stream object itself (so multiple Requests/Responses sharing the same stream observe
// the same `bodyUsed` behavior).
const BODY_STREAM_USED_KEY: &str = "__fastrender_body_stream_used";
const BODY_STREAM_ORIG_GET_READER_KEY: &str = "__fastrender_body_stream_orig_get_reader";
const BODY_STREAM_ORIG_CANCEL_KEY: &str = "__fastrender_body_stream_orig_cancel";
const BODY_STREAM_READER_STREAM_KEY: &str = "__fastrender_body_stream_reader_stream";
const BODY_STREAM_READER_ORIG_READ_KEY: &str = "__fastrender_body_stream_reader_orig_read";
const BODY_STREAM_READER_ORIG_CANCEL_KEY: &str = "__fastrender_body_stream_reader_orig_cancel";

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

const FETCH_HEADERS_HOST_TAG: u64 = 0x4645_5443_4848_4452; // "FETCHHDR"
const FETCH_HEADERS_ITERATOR_HOST_TAG: u64 = 0x4643_4848_4452_4954; // "FCHHDRIT"
const FETCH_REQUEST_HOST_TAG: u64 = 0x4645_5443_4852_4551; // "FETCHREQ"
const FETCH_RESPONSE_HOST_TAG: u64 = 0x4645_5443_4852_5350; // "FETCHRSP"

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

enum StreamConsumeKind {
  Text,
  ArrayBuffer,
  Bytes,
  Json,
  Blob { blob_type: String },
  FormData { content_type: Option<String> },
}

struct StreamConsumeState {
  stream_root: RootId,
  reader_root: RootId,
  resolve_root: RootId,
  reject_root: RootId,
  abort_signal_root: Option<RootId>,
  on_fulfilled_root: RootId,
  on_rejected_root: RootId,
  bytes: Vec<u8>,
  max_bytes: usize,
  kind: StreamConsumeKind,
}

struct EnvState {
  env: WindowFetchEnv,
  realm_id: RealmId,
  global: WeakGcObject,
  promise_executor_call: NativeFunctionId,
  body_stream_get_reader_wrapper_call: NativeFunctionId,
  body_stream_cancel_wrapper_call: NativeFunctionId,
  body_stream_reader_read_wrapper_call: NativeFunctionId,
  body_stream_reader_cancel_wrapper_call: NativeFunctionId,
  stream_consume_fulfilled_call: NativeFunctionId,
  stream_consume_rejected_call: NativeFunctionId,
  fetch_body_stream_then_fulfilled_call: NativeFunctionId,
  fetch_body_stream_then_rejected_call: NativeFunctionId,
  next_id: u64,
  multipart_boundary_counter: u64,
  owned_headers: HashMap<u64, CoreHeaders>,
  requests: HashMap<u64, CoreRequest>,
  responses: HashMap<u64, CoreResponse>,
  headers_iterators: HashMap<u64, HeadersIteratorState>,
  request_body_stream_wrappers: HashMap<u64, WeakGcObject>,
  response_body_stream_wrappers: HashMap<u64, WeakGcObject>,
  owned_headers_wrappers: HashMap<u64, WeakGcObject>,
  request_wrappers: HashMap<u64, RequestWrapperState>,
  response_wrappers: HashMap<u64, ResponseWrapperState>,
  headers_iterators_wrappers: HashMap<u64, WeakGcObject>,
  stream_consumptions: HashMap<u64, StreamConsumeState>,
  pending_fetch_stream_bodies: HashMap<u64, PendingFetchStreamBodyState>,
  last_gc_runs: u64,
}

struct PendingFetchStreamBodyState {
  request: CoreRequest,
  object_url_origin: String,
  headers_proto: GcObject,
  response_proto: GcObject,
  resolve_root: RootId,
  reject_root: RootId,
  promise_root: RootId,
  signal_root: Option<RootId>,
}

struct HeadersIteratorState {
  pairs: Vec<(String, String)>,
  index: usize,
}

#[derive(Clone, Copy)]
struct RequestWrapperState {
  request: WeakGcObject,
  headers: WeakGcObject,
}

#[derive(Clone, Copy)]
struct ResponseWrapperState {
  response: WeakGcObject,
  headers: WeakGcObject,
}

impl EnvState {
  fn new(
    env: WindowFetchEnv,
    realm_id: RealmId,
    global: WeakGcObject,
    promise_executor_call: NativeFunctionId,
    body_stream_get_reader_wrapper_call: NativeFunctionId,
    body_stream_cancel_wrapper_call: NativeFunctionId,
    body_stream_reader_read_wrapper_call: NativeFunctionId,
    body_stream_reader_cancel_wrapper_call: NativeFunctionId,
    stream_consume_fulfilled_call: NativeFunctionId,
    stream_consume_rejected_call: NativeFunctionId,
    fetch_body_stream_then_fulfilled_call: NativeFunctionId,
    fetch_body_stream_then_rejected_call: NativeFunctionId,
    last_gc_runs: u64,
  ) -> Self {
    Self {
      env,
      realm_id,
      global,
      promise_executor_call,
      body_stream_get_reader_wrapper_call,
      body_stream_cancel_wrapper_call,
      body_stream_reader_read_wrapper_call,
      body_stream_reader_cancel_wrapper_call,
      stream_consume_fulfilled_call,
      stream_consume_rejected_call,
      fetch_body_stream_then_fulfilled_call,
      fetch_body_stream_then_rejected_call,
      next_id: 1,
      multipart_boundary_counter: 1,
      owned_headers: HashMap::new(),
      requests: HashMap::new(),
      responses: HashMap::new(),
      headers_iterators: HashMap::new(),
      request_body_stream_wrappers: HashMap::new(),
      response_body_stream_wrappers: HashMap::new(),
      owned_headers_wrappers: HashMap::new(),
      request_wrappers: HashMap::new(),
      response_wrappers: HashMap::new(),
      headers_iterators_wrappers: HashMap::new(),
      stream_consumptions: HashMap::new(),
      pending_fetch_stream_bodies: HashMap::new(),
      last_gc_runs,
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
  let mut lock = envs()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
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

fn sweep_env_state_if_gc_ran_locked(state: &mut EnvState, heap: &Heap) {
  let gc_runs = heap.gc_runs();
  if gc_runs == state.last_gc_runs {
    return;
  }
  state.last_gc_runs = gc_runs;

  // Sweep cached `Request.body` / `Response.body` stream wrappers.
  //
  // These are `WeakGcObject`s so they do not keep streams alive on the Rust side; instead, the JS
  // wrappers determine liveness. The backing `CoreRequest`/`CoreResponse` must be retained while a
  // body stream wrapper is alive, since the stream's lazy init closure reads from the backing
  // object on first `read()` / `cancel()`.
  state
    .request_body_stream_wrappers
    .retain(|_, weak| weak.upgrade(heap).is_some());
  state
    .response_body_stream_wrappers
    .retain(|_, weak| weak.upgrade(heap).is_some());

  // Sweep Request-backed state.
  let requests = &mut state.requests;
  let request_body_stream_wrappers = &mut state.request_body_stream_wrappers;
  state.request_wrappers.retain(|request_id, wrapper| {
    let body_stream_alive = request_body_stream_wrappers.contains_key(request_id);

    if wrapper.request.upgrade(heap).is_some()
      || wrapper.headers.upgrade(heap).is_some()
      || body_stream_alive
    {
      true
    } else {
      requests.remove(request_id);
      request_body_stream_wrappers.remove(request_id);
      false
    }
  });

  // Sweep Response-backed state.
  let responses = &mut state.responses;
  let response_body_stream_wrappers = &mut state.response_body_stream_wrappers;
  state.response_wrappers.retain(|response_id, wrapper| {
    let body_stream_alive = response_body_stream_wrappers.contains_key(response_id);

    if wrapper.response.upgrade(heap).is_some()
      || wrapper.headers.upgrade(heap).is_some()
      || body_stream_alive
    {
      true
    } else {
      responses.remove(response_id);
      response_body_stream_wrappers.remove(response_id);
      false
    }
  });

  // Sweep owned Headers.
  let owned_headers = &mut state.owned_headers;
  state.owned_headers_wrappers.retain(|headers_id, weak| {
    if weak.upgrade(heap).is_some() {
      true
    } else {
      owned_headers.remove(headers_id);
      false
    }
  });

  // Sweep Headers iterators.
  let headers_iterators = &mut state.headers_iterators;
  state.headers_iterators_wrappers.retain(|iter_id, weak| {
    if weak.upgrade(heap).is_some() {
      true
    } else {
      headers_iterators.remove(iter_id);
      false
    }
  });
}

fn sweep_env_state_if_gc_ran(env_id: u64, heap: &Heap) -> Result<(), VmError> {
  let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get_mut(&env_id)
    .ok_or(VmError::Unimplemented("fetch env id not registered"))?;
  sweep_env_state_if_gc_ran_locked(state, heap);
  Ok(())
}

fn with_env_state<R>(
  env_id: u64,
  heap: &Heap,
  f: impl FnOnce(&EnvState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get_mut(&env_id)
    .ok_or(VmError::Unimplemented("fetch env id not registered"))?;
  sweep_env_state_if_gc_ran_locked(state, heap);
  f(state)
}

fn with_env_state_mut<R>(
  env_id: u64,
  heap: &Heap,
  f: impl FnOnce(&mut EnvState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let mut lock = envs()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get_mut(&env_id)
    .ok_or(VmError::Unimplemented("fetch env id not registered"))?;
  sweep_env_state_if_gc_ran_locked(state, heap);
  f(state)
}

fn with_env_state_no_sweep<R>(
  env_id: u64,
  f: impl FnOnce(&EnvState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get(&env_id)
    .ok_or(VmError::Unimplemented("fetch env id not registered"))?;
  f(state)
}

fn with_env_state_mut_no_sweep<R>(
  env_id: u64,
  f: impl FnOnce(&mut EnvState) -> Result<R, VmError>,
) -> Result<R, VmError> {
  let mut lock = envs()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
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

fn current_document_base_url(vm: &mut Vm, heap: &Heap, env_id: u64) -> Result<Option<String>, VmError> {
  if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
    // `document.baseURI` falls back to the realm document URL when the embedder has not installed a
    // base URL (or explicitly cleared it). Keep fetch's relative URL resolution consistent with
    // `document.baseURI` by treating a missing base URL as "use document_url".
    return Ok(Some(
      data
        .base_url
        .clone()
        .unwrap_or_else(|| data.document_url().to_string()),
    ));
  }
  with_env_state(env_id, heap, |state| Ok(state.env.document_url.clone()))
}

fn serialized_origin_for_document_url(url: &str) -> String {
  let Ok(url) = ::url::Url::parse(url) else {
    return "null".to_string();
  };
  match url.scheme() {
    "http" | "https" => url.origin().ascii_serialization(),
    _ => "null".to_string(),
  }
}

fn current_document_origin_for_object_urls(
  vm: &mut Vm,
  heap: &Heap,
  env_id: u64,
) -> Result<String, VmError> {
  if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
    return Ok(serialized_origin_for_document_url(data.document_url()));
  }
  with_env_state(env_id, heap, |state| {
    Ok(
      state
        .env
        .document_url
        .as_deref()
        .map(serialized_origin_for_document_url)
        .unwrap_or_else(|| "null".to_string()),
    )
  })
}

fn execute_blob_url_fetch(
  request: &CoreRequest,
  current_origin: &str,
) -> crate::error::Result<CoreResponse> {
  if !(request.method.eq_ignore_ascii_case("GET") || request.method.eq_ignore_ascii_case("HEAD")) {
    return Err(crate::error::Error::Other(
      "blob: URL fetch only supports GET/HEAD".to_string(),
    ));
  }

  let Some(entry) = window_object_url::get_object_url(&request.url) else {
    return Err(crate::error::Error::Other(
      "blob: URL not found (revoked?)".to_string(),
    ));
  };

  if entry.origin != current_origin {
    return Err(crate::error::Error::Other(
      "blob: URL origin does not match current origin".to_string(),
    ));
  }

  let mut response = CoreResponse::new(200);
  response.r#type = ResponseType::Basic;
  response.url = request.url.clone();
  response.headers =
    CoreHeaders::new_with_guard_and_limits(HeadersGuard::Response, request.headers.limits());

  if !entry.content_type.is_empty() {
    response
      .headers
      .append("Content-Type", &entry.content_type)
      .map_err(|e| crate::error::Error::Other(e.to_string()))?;
  }

  if request.method.eq_ignore_ascii_case("HEAD") {
    response.body = None;
    return Ok(response);
  }

  response.body = Some(
    Body::new_response(entry.bytes, response.headers.limits())
      .map_err(|e| crate::error::Error::Other(e.to_string()))?,
  );

  Ok(response)
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

fn request_wrapper_cached_body_stream_is_locked(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  request: GcObject,
) -> Result<bool, VmError> {
  // Task 85 stores the `Request.body` stream on the instance (so future reads return the same
  // stream object). When cloning a Request without overriding `init.body`, the Fetch spec requires
  // throwing if the input body stream is locked.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(request))?;
  let body_stream = get_data_prop(&mut scope, request, REQUEST_BODY_STREAM_KEY)?;
  let Value::Object(body_stream_obj) = body_stream else {
    return Ok(false);
  };
  readable_stream_is_locked(vm, &mut scope, host, host_hooks, body_stream_obj)
}

fn request_body_stream_locked(
  vm: &Vm,
  env_id: u64,
  request_id: u64,
  heap: &mut Heap,
) -> Result<bool, VmError> {
  let stream_obj = {
    let heap_ref: &Heap = &*heap;
    with_env_state(env_id, heap_ref, |state| {
      let Some(weak) = state.request_body_stream_wrappers.get(&request_id) else {
        return Ok(None);
      };
      Ok(weak.upgrade(heap_ref))
    })?
  };
  let Some(stream_obj) = stream_obj else {
    return Ok(false);
  };
  Ok(window_streams::readable_stream_is_locked(vm, heap, stream_obj))
}

fn response_wrapper_cached_body_stream_is_locked(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  response: GcObject,
) -> Result<bool, VmError> {
  // Like `Request.body`, `Response.body` caches the returned stream object on the instance.
  // When consuming the body via `text()` / `json()` / etc, Fetch requires rejecting if the cached
  // stream is locked.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(response))?;
  let body_stream = get_data_prop(&mut scope, response, RESPONSE_BODY_STREAM_KEY)?;
  let Value::Object(body_stream_obj) = body_stream else {
    return Ok(false);
  };
  readable_stream_is_locked(vm, &mut scope, host, host_hooks, body_stream_obj)
}

fn body_stream_is_used(scope: &mut Scope<'_>, stream: GcObject) -> Result<bool, VmError> {
  Ok(matches!(
    get_data_prop(scope, stream, BODY_STREAM_USED_KEY)?,
    Value::Bool(true)
  ))
}

fn body_stream_set_used(scope: &mut Scope<'_>, stream: GcObject, used: bool) -> Result<(), VmError> {
  set_data_prop(scope, stream, BODY_STREAM_USED_KEY, Value::Bool(used), /* writable */ true)
}

fn is_readable_stream_like_object(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  obj: GcObject,
) -> Result<bool, VmError> {
  if window_streams::is_readable_stream_object(vm, scope.heap_mut(), obj) {
    return Ok(true);
  }

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;

  let get_reader_key = alloc_key(&mut scope, "getReader")?;
  let get_reader = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, obj, get_reader_key)?;
  if !scope.heap().is_callable(get_reader).unwrap_or(false) {
    return Ok(false);
  }

  let cancel_key = alloc_key(&mut scope, "cancel")?;
  let cancel = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, obj, cancel_key)?;
  if !scope.heap().is_callable(cancel).unwrap_or(false) {
    return Ok(false);
  }

  Ok(true)
}

fn readable_stream_is_locked(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  stream: GcObject,
) -> Result<bool, VmError> {
  // `window_streams::readable_stream_is_locked` only works for streams created by
  // `crate::js::window_streams`. For user-created streams, fall back to reading the `.locked`
  // getter.
  if window_streams::is_readable_stream_object(vm, scope.heap_mut(), stream) {
    return Ok(window_streams::readable_stream_is_locked(vm, scope.heap_mut(), stream));
  }

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(stream))?;

  let locked_key = alloc_key(&mut scope, "locked")?;
  let locked_val = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, stream, locked_key)?;
  Ok(scope.heap().to_boolean(locked_val)?)
}

fn ensure_body_stream_wrappers_installed(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  env_id: u64,
  stream: GcObject,
) -> Result<(), VmError> {
  // Initialize the used flag if absent.
  if matches!(
    get_data_prop(scope, stream, BODY_STREAM_USED_KEY)?,
    Value::Undefined
  ) {
    body_stream_set_used(scope, stream, false)?;
  }

  let already_wrapped = !matches!(
    get_data_prop(scope, stream, BODY_STREAM_ORIG_GET_READER_KEY)?,
    Value::Undefined
  );
  if already_wrapped {
    return Ok(());
  }

  // Load original methods (from the prototype) and replace with wrappers that mark disturbed state.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(stream))?;

  let get_reader_key = alloc_key(&mut scope, "getReader")?;
  let get_reader = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, stream, get_reader_key)?;
  if !scope.heap().is_callable(get_reader).unwrap_or(false) {
    return Err(VmError::TypeError("ReadableStream.getReader is not callable"));
  }
  set_data_prop(&mut scope, stream, BODY_STREAM_ORIG_GET_READER_KEY, get_reader, false)?;

  let cancel_key = alloc_key(&mut scope, "cancel")?;
  let cancel = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, stream, cancel_key)?;
  if !scope.heap().is_callable(cancel).unwrap_or(false) {
    return Err(VmError::TypeError("ReadableStream.cancel is not callable"));
  }
  set_data_prop(&mut scope, stream, BODY_STREAM_ORIG_CANCEL_KEY, cancel, false)?;

  let (
    get_reader_call,
    cancel_call,
  ) = with_env_state(env_id, scope.heap(), |state| {
    Ok((
      state.body_stream_get_reader_wrapper_call,
      state.body_stream_cancel_wrapper_call,
    ))
  })?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let func_proto = intr.function_prototype();

  let get_reader_name = scope.alloc_string("getReader")?;
  scope.push_root(Value::String(get_reader_name))?;
  let get_reader_wrapper = scope.alloc_native_function_with_slots(
    get_reader_call,
    None,
    get_reader_name,
    0,
    &[Value::Number(env_id as f64)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(get_reader_wrapper, Some(func_proto))?;
  set_data_prop(
    &mut scope,
    stream,
    "getReader",
    Value::Object(get_reader_wrapper),
    true,
  )?;

  let cancel_name = scope.alloc_string("cancel")?;
  scope.push_root(Value::String(cancel_name))?;
  let cancel_wrapper = scope.alloc_native_function_with_slots(
    cancel_call,
    None,
    cancel_name,
    1,
    &[Value::Number(env_id as f64)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(cancel_wrapper, Some(func_proto))?;
  set_data_prop(
    &mut scope,
    stream,
    "cancel",
    Value::Object(cancel_wrapper),
    true,
  )?;

  Ok(())
}

fn ensure_body_stream_reader_wrappers_installed(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  env_id: u64,
  stream: GcObject,
  reader: GcObject,
) -> Result<(), VmError> {
  let already_wrapped = !matches!(
    get_data_prop(scope, reader, BODY_STREAM_READER_ORIG_READ_KEY)?,
    Value::Undefined
  );
  if already_wrapped {
    return Ok(());
  }

  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(reader))?;
  scope.push_root(Value::Object(stream))?;

  set_data_prop(&mut scope, reader, BODY_STREAM_READER_STREAM_KEY, Value::Object(stream), false)?;

  let read_key = alloc_key(&mut scope, "read")?;
  let read = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, reader, read_key)?;
  if scope.heap().is_callable(read).unwrap_or(false) {
    set_data_prop(&mut scope, reader, BODY_STREAM_READER_ORIG_READ_KEY, read, false)?;

    let read_call = with_env_state(env_id, scope.heap(), |state| Ok(state.body_stream_reader_read_wrapper_call))?;
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    let func_proto = intr.function_prototype();
    let name = scope.alloc_string("read")?;
    scope.push_root(Value::String(name))?;
    let wrapper = scope.alloc_native_function_with_slots(read_call, None, name, 0, &[Value::Number(env_id as f64)])?;
    scope
      .heap_mut()
      .object_set_prototype(wrapper, Some(func_proto))?;
    set_data_prop(&mut scope, reader, "read", Value::Object(wrapper), true)?;
  }

  let cancel_key = alloc_key(&mut scope, "cancel")?;
  let cancel = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, reader, cancel_key)?;
  if scope.heap().is_callable(cancel).unwrap_or(false) {
    set_data_prop(&mut scope, reader, BODY_STREAM_READER_ORIG_CANCEL_KEY, cancel, false)?;

    let cancel_call =
      with_env_state(env_id, scope.heap(), |state| Ok(state.body_stream_reader_cancel_wrapper_call))?;
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    let func_proto = intr.function_prototype();
    let name = scope.alloc_string("cancel")?;
    scope.push_root(Value::String(name))?;
    let wrapper = scope.alloc_native_function_with_slots(cancel_call, None, name, 1, &[Value::Number(env_id as f64)])?;
    scope
      .heap_mut()
      .object_set_prototype(wrapper, Some(func_proto))?;
    set_data_prop(&mut scope, reader, "cancel", Value::Object(wrapper), true)?;
  }

  Ok(())
}

fn readable_stream_tee(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  stream: GcObject,
) -> Result<(GcObject, GcObject), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(stream))?;

  let tee_key = alloc_key(&mut scope, "tee")?;
  let tee = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, stream, tee_key)?;
  if !scope.heap().is_callable(tee).unwrap_or(false) {
    return Err(VmError::TypeError(
      "Cannot clone a streaming body (not implemented)",
    ));
  }

  let result = vm.call_with_host_and_hooks(host, &mut scope, host_hooks, tee, Value::Object(stream), &[])?;
  let Value::Object(arr_obj) = result else {
    return Err(VmError::TypeError(
      "ReadableStream.tee() must return an array-like object",
    ));
  };
  scope.push_root(Value::Object(arr_obj))?;

  let s0_key = alloc_key(&mut scope, "0")?;
  let s1_key = alloc_key(&mut scope, "1")?;
  let s0 = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, arr_obj, s0_key)?;
  let s1 = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, arr_obj, s1_key)?;
  match (s0, s1) {
    (Value::Object(a), Value::Object(b)) => Ok((a, b)),
    _ => Err(VmError::TypeError(
      "ReadableStream.tee() must return two ReadableStream objects",
    )),
  }
}

fn start_consuming_body_stream(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  env_id: u64,
  stream: GcObject,
  abort_signal: Option<GcObject>,
  max_bytes: usize,
  kind: StreamConsumeKind,
) -> Result<Value, VmError> {
  let cap = new_promise_capability_for_env(vm, scope, host, host_hooks, env_id)?;
  let promise = cap.promise;

  // Root resolve/reject and stream for the duration of consumption.
  let resolve_root = scope.heap_mut().add_root(cap.resolve)?;
  let reject_root = scope.heap_mut().add_root(cap.reject)?;
  let stream_root = scope.heap_mut().add_root(Value::Object(stream))?;
  let abort_signal_root = match abort_signal {
    Some(obj) => Some(scope.heap_mut().add_root(Value::Object(obj))?),
    None => None,
  };

  // Install wrappers for `getReader()`/`cancel()` and reader methods.
  ensure_body_stream_wrappers_installed(vm, scope, host, host_hooks, env_id, stream)?;

  // Acquire a reader and kick off the first read.
  let reader = (|| -> Result<GcObject, VmError> {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(stream))?;
    let get_reader_key = alloc_key(&mut scope, "getReader")?;
    let get_reader =
      vm.get_with_host_and_hooks(host, &mut scope, host_hooks, stream, get_reader_key)?;
    let reader = vm.call_with_host_and_hooks(
      host,
      &mut scope,
      host_hooks,
      get_reader,
      Value::Object(stream),
      &[],
    )?;
    match reader {
      Value::Object(reader_obj) => Ok(reader_obj),
      _ => Err(VmError::TypeError("ReadableStream.getReader must return an object")),
    }
  })();

  let reader = match reader {
    Ok(reader) => reader,
    Err(err) => {
      // Reject the returned Promise rather than throwing synchronously.
      let err_value = match err {
        VmError::Throw(thrown) => thrown,
        other => create_type_error(vm, scope, host, host_hooks, &other.to_string())?,
      };
      vm.call_with_host_and_hooks(
        host,
        scope,
        host_hooks,
        cap.reject,
        Value::Undefined,
        &[err_value],
      )?;
      scope.heap_mut().remove_root(resolve_root);
      scope.heap_mut().remove_root(reject_root);
      scope.heap_mut().remove_root(stream_root);
      if let Some(abort_signal_root) = abort_signal_root {
        scope.heap_mut().remove_root(abort_signal_root);
      }
      return Ok(promise);
    }
  };

  ensure_body_stream_reader_wrappers_installed(vm, scope, host, host_hooks, env_id, stream, reader)?;

  let reader_root = scope.heap_mut().add_root(Value::Object(reader))?;

  // Allocate a consumption id and callback function objects.
  let (consume_id, on_fulfilled_call, on_rejected_call) =
    with_env_state_mut(env_id, scope.heap(), |state| {
      let id = state.alloc_id();
      Ok((id, state.stream_consume_fulfilled_call, state.stream_consume_rejected_call))
    })?;

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let func_proto = intr.function_prototype();

  let on_fulfilled_name = scope.alloc_string("onStreamRead")?;
  scope.push_root(Value::String(on_fulfilled_name))?;
  let on_fulfilled = scope.alloc_native_function_with_slots(
    on_fulfilled_call,
    None,
    on_fulfilled_name,
    1,
    &[Value::Number(env_id as f64), Value::Number(consume_id as f64)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(on_fulfilled, Some(func_proto))?;

  let on_rejected_name = scope.alloc_string("onStreamReadRejected")?;
  scope.push_root(Value::String(on_rejected_name))?;
  let on_rejected = scope.alloc_native_function_with_slots(
    on_rejected_call,
    None,
    on_rejected_name,
    1,
    &[Value::Number(env_id as f64), Value::Number(consume_id as f64)],
  )?;
  scope
    .heap_mut()
    .object_set_prototype(on_rejected, Some(func_proto))?;

  let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
  let on_rejected_root = scope.heap_mut().add_root(Value::Object(on_rejected))?;

  with_env_state_mut(env_id, scope.heap(), |state| {
    state.stream_consumptions.insert(
      consume_id,
      StreamConsumeState {
        stream_root,
        reader_root,
        resolve_root,
        reject_root,
        abort_signal_root,
        on_fulfilled_root,
        on_rejected_root,
        bytes: Vec::new(),
        max_bytes,
        kind,
      },
    );
    Ok(())
  })?;

  // reader.read().then(on_fulfilled, on_rejected)
  let start_result: Result<(), VmError> = (|| {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(reader))?;
    scope.push_root(Value::Object(on_fulfilled))?;
    scope.push_root(Value::Object(on_rejected))?;

    let read_key = alloc_key(&mut scope, "read")?;
    let read_fn = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, reader, read_key)?;
    let promise_val = vm.call_with_host_and_hooks(
      host,
      &mut scope,
      host_hooks,
      read_fn,
      Value::Object(reader),
      &[],
    )?;
    let Value::Object(promise_obj) = promise_val else {
      return Err(VmError::TypeError("ReadableStream reader.read must return a Promise"));
    };
    scope.push_root(Value::Object(promise_obj))?;

    let then_key = alloc_key(&mut scope, "then")?;
    let then_fn = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, promise_obj, then_key)?;
    vm.call_with_host_and_hooks(
      host,
      &mut scope,
      host_hooks,
      then_fn,
      Value::Object(promise_obj),
      &[Value::Object(on_fulfilled), Value::Object(on_rejected)],
    )?;
    Ok(())
  })();

  if let Err(err) = start_result {
    // Fail fast: remove the consumption state and reject the Promise.
    let removed = with_env_state_mut(env_id, scope.heap(), |state| {
      Ok(state.stream_consumptions.remove(&consume_id))
    })?;
    if let Some(state) = removed {
      scope.heap_mut().remove_root(state.stream_root);
      scope.heap_mut().remove_root(state.reader_root);
      scope.heap_mut().remove_root(state.resolve_root);
      scope.heap_mut().remove_root(state.reject_root);
      if let Some(abort_signal_root) = state.abort_signal_root {
        scope.heap_mut().remove_root(abort_signal_root);
      }
      scope.heap_mut().remove_root(state.on_fulfilled_root);
      scope.heap_mut().remove_root(state.on_rejected_root);
    }

    let err_value = match err {
      VmError::Throw(thrown) => thrown,
      other => create_type_error(vm, scope, host, host_hooks, &other.to_string())?,
    };
    vm.call_with_host_and_hooks(
      host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
  }

  Ok(promise)
}

fn body_stream_get_reader_wrapper_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let Value::Object(stream) = this else {
    return Err(VmError::TypeError("ReadableStream.getReader: illegal invocation"));
  };

  let original = get_data_prop(scope, stream, BODY_STREAM_ORIG_GET_READER_KEY)?;
  if !scope.heap().is_callable(original).unwrap_or(false) {
    return Err(VmError::TypeError("ReadableStream.getReader: missing original getReader"));
  }

  let reader_val = vm.call_with_host_and_hooks(host, scope, host_hooks, original, this, args)?;
  if let Value::Object(reader) = reader_val {
    ensure_body_stream_reader_wrappers_installed(vm, scope, host, host_hooks, env_id, stream, reader)?;
  }
  Ok(reader_val)
}

fn body_stream_cancel_wrapper_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(stream) = this else {
    return Err(VmError::TypeError("ReadableStream.cancel: illegal invocation"));
  };

  let original = get_data_prop(scope, stream, BODY_STREAM_ORIG_CANCEL_KEY)?;
  if !scope.heap().is_callable(original).unwrap_or(false) {
    return Err(VmError::TypeError("ReadableStream.cancel: missing original cancel"));
  }

  let result = vm.call_with_host_and_hooks(host, scope, host_hooks, original, this, args)?;
  body_stream_set_used(scope, stream, true)?;
  Ok(result)
}

fn body_stream_reader_read_wrapper_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(reader) = this else {
    return Err(VmError::TypeError("ReadableStreamDefaultReader.read: illegal invocation"));
  };

  let Value::Object(stream) = get_data_prop(scope, reader, BODY_STREAM_READER_STREAM_KEY)? else {
    return Err(VmError::TypeError(
      "ReadableStreamDefaultReader.read: missing backing stream",
    ));
  };

  let original = get_data_prop(scope, reader, BODY_STREAM_READER_ORIG_READ_KEY)?;
  if !scope.heap().is_callable(original).unwrap_or(false) {
    return Err(VmError::TypeError(
      "ReadableStreamDefaultReader.read: missing original read",
    ));
  }

  let result = vm.call_with_host_and_hooks(host, scope, host_hooks, original, this, args)?;
  body_stream_set_used(scope, stream, true)?;
  Ok(result)
}

fn body_stream_reader_cancel_wrapper_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(reader) = this else {
    return Err(VmError::TypeError(
      "ReadableStreamDefaultReader.cancel: illegal invocation",
    ));
  };

  let Value::Object(stream) = get_data_prop(scope, reader, BODY_STREAM_READER_STREAM_KEY)? else {
    return Err(VmError::TypeError(
      "ReadableStreamDefaultReader.cancel: missing backing stream",
    ));
  };

  let original = get_data_prop(scope, reader, BODY_STREAM_READER_ORIG_CANCEL_KEY)?;
  if !scope.heap().is_callable(original).unwrap_or(false) {
    return Err(VmError::TypeError(
      "ReadableStreamDefaultReader.cancel: missing original cancel",
    ));
  }

  let result = vm.call_with_host_and_hooks(host, scope, host_hooks, original, this, args)?;
  body_stream_set_used(scope, stream, true)?;
  Ok(result)
}

fn stream_consume_info_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<(u64, u64), VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let env_id = slots
    .get(0)
    .copied()
    .ok_or(VmError::InvariantViolation(
      "stream consume callback missing env id slot",
    ))?;
  let consume_id = slots
    .get(1)
    .copied()
    .ok_or(VmError::InvariantViolation(
      "stream consume callback missing consume id slot",
    ))?;
  let env_id = number_to_u64(env_id).map_err(|_| VmError::InvariantViolation("invalid env id slot"))?;
  let consume_id =
    number_to_u64(consume_id).map_err(|_| VmError::InvariantViolation("invalid consume id slot"))?;
  Ok((env_id, consume_id))
}

fn stream_consume_cleanup(scope: &mut Scope<'_>, env_id: u64, consume_id: u64) -> Result<Option<StreamConsumeState>, VmError> {
  with_env_state_mut(env_id, scope.heap(), |state| Ok(state.stream_consumptions.remove(&consume_id)))
}

fn stream_consume_release_lock_best_effort(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  reader: GcObject,
) {
  let mut scope = scope.reborrow();
  if scope.push_root(Value::Object(reader)).is_err() {
    return;
  }
  let release_key = match alloc_key(&mut scope, "releaseLock") {
    Ok(k) => k,
    Err(_) => return,
  };
  let release = match vm.get_with_host_and_hooks(host, &mut scope, host_hooks, reader, release_key) {
    Ok(v) => v,
    Err(_) => return,
  };
  if !scope.heap().is_callable(release).unwrap_or(false) {
    return;
  }
  let _ = vm.call_with_host_and_hooks(host, &mut scope, host_hooks, release, Value::Object(reader), &[]);
}

fn stream_consume_resolve_reject_and_cleanup(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  mut state: StreamConsumeState,
  outcome: Result<Value, Value>,
) -> Result<(), VmError> {
  // Best-effort unlock so subsequent calls see BodyUsed instead of locked.
  let reader_val = scope
    .heap()
    .get_root(state.reader_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  if let Value::Object(reader_obj) = reader_val {
    stream_consume_release_lock_best_effort(vm, scope, host, host_hooks, reader_obj);
  }

  let resolve = scope
    .heap()
    .get_root(state.resolve_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let reject = scope
    .heap()
    .get_root(state.reject_root)
    .ok_or_else(|| VmError::invalid_handle())?;

  match outcome {
    Ok(value) => {
      vm.call_with_host_and_hooks(host, scope, host_hooks, resolve, Value::Undefined, &[value])?;
    }
    Err(reason) => {
      vm.call_with_host_and_hooks(host, scope, host_hooks, reject, Value::Undefined, &[reason])?;
    }
  }

  scope.heap_mut().remove_root(state.stream_root);
  scope.heap_mut().remove_root(state.reader_root);
  scope.heap_mut().remove_root(state.resolve_root);
  scope.heap_mut().remove_root(state.reject_root);
  if let Some(abort_signal_root) = state.abort_signal_root {
    scope.heap_mut().remove_root(abort_signal_root);
  }
  scope.heap_mut().remove_root(state.on_fulfilled_root);
  scope.heap_mut().remove_root(state.on_rejected_root);
  // Drop buffers eagerly.
  state.bytes.clear();
  Ok(())
}

fn stream_consume_fulfilled_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, consume_id) = stream_consume_info_from_callee(scope, callee)?;

  // AbortSignal support (used by fetch request body buffering). If a signal is provided and has
  // been aborted, stop consumption and reject with `signal.reason`.
  let abort_signal_root = with_env_state(env_id, scope.heap(), |state| {
    Ok(
      state
        .stream_consumptions
        .get(&consume_id)
        .and_then(|s| s.abort_signal_root),
    )
  })?;
  if let Some(signal_root_id) = abort_signal_root {
    let signal_value = scope
      .heap()
      .get_root(signal_root_id)
      .ok_or_else(|| VmError::invalid_handle())?;
    if let Value::Object(signal_obj) = signal_value {
      scope.push_root(Value::Object(signal_obj))?;
      let aborted_key = alloc_key(scope, "aborted")?;
      let aborted_val =
        vm.get_with_host_and_hooks(host, scope, host_hooks, signal_obj, aborted_key)?;
      if scope.heap().to_boolean(aborted_val)? {
        let reason_key = alloc_key(scope, "reason")?;
        let reason = vm.get_with_host_and_hooks(host, scope, host_hooks, signal_obj, reason_key)?;
        if let Some(state) = stream_consume_cleanup(scope, env_id, consume_id)? {
          stream_consume_resolve_reject_and_cleanup(vm, scope, host, host_hooks, state, Err(reason))?;
        }
        return Ok(Value::Undefined);
      }
    }
  }

  let result_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(result_obj) = result_val else {
    if let Some(state) = stream_consume_cleanup(scope, env_id, consume_id)? {
      let err = create_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "ReadableStream reader.read() must resolve to an object",
      )?;
      stream_consume_resolve_reject_and_cleanup(vm, scope, host, host_hooks, state, Err(err))?;
    }
    return Ok(Value::Undefined);
  };

  scope.push_root(Value::Object(result_obj))?;
  let done_key = alloc_key(scope, "done")?;
  let value_key = alloc_key(scope, "value")?;
  let done_val = vm.get_with_host_and_hooks(host, scope, host_hooks, result_obj, done_key)?;
  let done = scope.heap().to_boolean(done_val)?;

  if done {
    let Some(mut state) = stream_consume_cleanup(scope, env_id, consume_id)? else {
      return Ok(Value::Undefined);
    };

    let outcome: Result<Value, Value> = match &state.kind {
      StreamConsumeKind::Text => {
        let mut bytes = std::mem::take(&mut state.bytes);
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
          bytes.drain(..3);
        }
        let text = match String::from_utf8(bytes) {
          Ok(s) => s,
          Err(err) => String::from_utf8_lossy(&err.into_bytes()).into_owned(),
        };
        let s = scope.alloc_string(&text)?;
        Ok(Value::String(s))
      }
      StreamConsumeKind::ArrayBuffer => {
        let bytes = std::mem::take(&mut state.bytes);
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
        scope
          .heap_mut()
          .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
        Ok(Value::Object(ab))
      }
      StreamConsumeKind::Bytes => {
        let bytes = std::mem::take(&mut state.bytes);
        let byte_len = bytes.len();
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
        scope.push_root(Value::Object(ab))?;
        scope
          .heap_mut()
          .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
        let view = scope.alloc_uint8_array(ab, 0, byte_len)?;
        scope
          .heap_mut()
          .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
        Ok(Value::Object(view))
      }
      StreamConsumeKind::Json => {
        let bytes = std::mem::take(&mut state.bytes);
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
          Ok(v) => Ok(json_to_js(vm, scope, &v)?),
          Err(err) => Err(create_syntax_error(vm, scope, host, host_hooks, &err.to_string())?),
        }
      }
      StreamConsumeKind::Blob { blob_type } => {
        let bytes = std::mem::take(&mut state.bytes);
        let realm_id = vm.current_realm().ok_or(VmError::Unimplemented(
          "Blob creation requires an active realm",
        ))?;
        let proto = window_blob::blob_prototype_for_realm(realm_id)
          .ok_or(VmError::Unimplemented("Blob bindings not installed"))?;
        let blob_obj = window_blob::create_blob_with_proto(
          vm,
          scope,
          callee,
          proto,
          window_blob::BlobData {
            bytes,
            r#type: blob_type.clone(),
          },
        )?;
        Ok(Value::Object(blob_obj))
      }
      StreamConsumeKind::FormData { content_type } => {
        let bytes = std::mem::take(&mut state.bytes);
        let file_last_modified_ms =
          if content_type
            .as_deref()
            .is_some_and(|ct| normalize_content_type_for_blob(ct) == "multipart/form-data")
          {
            crate::js::time::date_now_ms(scope)?
          } else {
            0
          };
        match parse_form_data_entries_from_body(content_type.as_deref(), &bytes, file_last_modified_ms) {
          Ok(entries) => {
            let fd_obj = window_form_data::create_form_data_with_entries(vm, scope, callee, entries)?;
            Ok(Value::Object(fd_obj))
          }
          Err(msg) => Err(create_type_error(vm, scope, host, host_hooks, msg)?),
        }
      }
    };

    match outcome {
      Ok(val) => stream_consume_resolve_reject_and_cleanup(vm, scope, host, host_hooks, state, Ok(val))?,
      Err(err) => stream_consume_resolve_reject_and_cleanup(vm, scope, host, host_hooks, state, Err(err))?,
    }
    return Ok(Value::Undefined);
  }

  let chunk_val = vm.get_with_host_and_hooks(host, scope, host_hooks, result_obj, value_key)?;
  let chunk_bytes: &[u8] = match chunk_val {
    Value::Object(obj) => {
      if scope.heap().is_uint8_array_object(obj) {
        scope.heap().uint8_array_data(obj)?
      } else if scope.heap().is_array_buffer_object(obj) {
        scope.heap().array_buffer_data(obj)?
      } else {
        let Some(state) = stream_consume_cleanup(scope, env_id, consume_id)? else {
          return Ok(Value::Undefined);
        };
        let err = create_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "ReadableStream chunk must be a Uint8Array or ArrayBuffer",
        )?;
        stream_consume_resolve_reject_and_cleanup(vm, scope, host, host_hooks, state, Err(err))?;
        return Ok(Value::Undefined);
      }
    }
    _ => {
      let Some(state) = stream_consume_cleanup(scope, env_id, consume_id)? else {
        return Ok(Value::Undefined);
      };
      let err = create_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "ReadableStream chunk must be a Uint8Array or ArrayBuffer",
      )?;
      stream_consume_resolve_reject_and_cleanup(vm, scope, host, host_hooks, state, Err(err))?;
      return Ok(Value::Undefined);
    }
  };

  enum AppendOutcome {
    Continue {
      reader_root: RootId,
      on_fulfilled_root: RootId,
      on_rejected_root: RootId,
    },
    TooLong(StreamConsumeState),
  }

  let append_outcome: AppendOutcome = with_env_state_mut(env_id, scope.heap(), |state| {
    let too_long = {
      let consume_state = state
        .stream_consumptions
        .get(&consume_id)
        .ok_or(VmError::TypeError("stream consume state missing"))?;
      let next_len = consume_state
        .bytes
        .len()
        .checked_add(chunk_bytes.len())
        .unwrap_or(usize::MAX);
      next_len > consume_state.max_bytes
    };

    if too_long {
      let removed = state
        .stream_consumptions
        .remove(&consume_id)
        .ok_or(VmError::TypeError("stream consume state missing"))?;
      return Ok(AppendOutcome::TooLong(removed));
    }

    let consume_state = state
      .stream_consumptions
      .get_mut(&consume_id)
      .ok_or(VmError::TypeError("stream consume state missing"))?;
    consume_state.bytes.extend_from_slice(chunk_bytes);
    Ok(AppendOutcome::Continue {
      reader_root: consume_state.reader_root,
      on_fulfilled_root: consume_state.on_fulfilled_root,
      on_rejected_root: consume_state.on_rejected_root,
    })
  })?;

  let (reader_root, on_fulfilled_root, on_rejected_root) = match append_outcome {
    AppendOutcome::Continue {
      reader_root,
      on_fulfilled_root,
      on_rejected_root,
    } => (reader_root, on_fulfilled_root, on_rejected_root),
    AppendOutcome::TooLong(state) => {
      let err_value = create_type_error(vm, scope, host, host_hooks, FETCH_BODY_TOO_LONG_ERROR)?;
      stream_consume_resolve_reject_and_cleanup(vm, scope, host, host_hooks, state, Err(err_value))?;
      return Ok(Value::Undefined);
    }
  };

  // Continue reading: reader.read().then(on_fulfilled, on_rejected)
  let reader_val = scope
    .heap()
    .get_root(reader_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let on_fulfilled_val = scope
    .heap()
    .get_root(on_fulfilled_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let on_rejected_val = scope
    .heap()
    .get_root(on_rejected_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let (Value::Object(reader_obj), Value::Object(on_fulfilled_obj), Value::Object(on_rejected_obj)) =
    (reader_val, on_fulfilled_val, on_rejected_val)
  else {
    return Ok(Value::Undefined);
  };

  let _ = (|| -> Result<(), VmError> {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(reader_obj))?;
    scope.push_root(Value::Object(on_fulfilled_obj))?;
    scope.push_root(Value::Object(on_rejected_obj))?;

    let read_key = alloc_key(&mut scope, "read")?;
    let read_fn = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, reader_obj, read_key)?;
    let promise_val = vm.call_with_host_and_hooks(
      host,
      &mut scope,
      host_hooks,
      read_fn,
      Value::Object(reader_obj),
      &[],
    )?;
    let Value::Object(promise_obj) = promise_val else {
      return Err(VmError::TypeError("ReadableStream reader.read must return a Promise"));
    };
    scope.push_root(Value::Object(promise_obj))?;

    let then_key = alloc_key(&mut scope, "then")?;
    let then_fn =
      vm.get_with_host_and_hooks(host, &mut scope, host_hooks, promise_obj, then_key)?;
    vm.call_with_host_and_hooks(
      host,
      &mut scope,
      host_hooks,
      then_fn,
      Value::Object(promise_obj),
      &[Value::Object(on_fulfilled_obj), Value::Object(on_rejected_obj)],
    )?;
    Ok(())
  })();

  Ok(Value::Undefined)
}

fn stream_consume_rejected_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, consume_id) = stream_consume_info_from_callee(scope, callee)?;
  let Some(state) = stream_consume_cleanup(scope, env_id, consume_id)? else {
    return Ok(Value::Undefined);
  };
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  stream_consume_resolve_reject_and_cleanup(vm, scope, host, host_hooks, state, Err(reason))?;
  Ok(Value::Undefined)
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
const FETCH_CACHE_TOO_LONG_ERROR: &str = "Request.cache exceeds maximum length";
const FETCH_INTEGRITY_TOO_LONG_ERROR: &str = "Request.integrity exceeds maximum length";
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
  vm.construct_with_host_and_hooks(
    host,
    scope,
    hooks,
    Value::Object(ctor),
    &[Value::String(msg)],
    Value::Object(ctor),
  )
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
  match slots
    .get(SLOT_HEADERS_PROTO)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "fetch binding missing Headers.prototype native slot",
    )),
  }
}

fn response_proto_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(SLOT_RESPONSE_PROTO)
    .copied()
    .unwrap_or(Value::Undefined)
  {
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
  let Some(slots) = scope.heap().object_host_slots(obj).ok().flatten() else {
    return None;
  };
  if slots.a != FETCH_REQUEST_HOST_TAG {
    return None;
  }
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
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  if slots.a != FETCH_REQUEST_HOST_TAG {
    return Err(VmError::TypeError("Request: illegal invocation"));
  }

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
    _ => {
      return Err(VmError::InvariantViolation(
        "Promise executor missing capture slot",
      ))
    }
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

  let executor_call = with_env_state(env_id, scope.heap(), |state| Ok(state.promise_executor_call))?;

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
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError("Headers: illegal invocation"));
  };
  if slots.a != FETCH_HEADERS_HOST_TAG {
    return Err(VmError::TypeError("Headers: illegal invocation"));
  }
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
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  if slots.a != FETCH_RESPONSE_HOST_TAG {
    return Err(VmError::TypeError("Response: illegal invocation"));
  }

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

fn get_headers_ref<'a>(
  state: &'a EnvState,
  kind: u8,
  owner: u64,
) -> Result<&'a CoreHeaders, VmError> {
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
      let pairs = with_env_state(other_env, scope.heap(), |state| {
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
        return Err(throw_type_error(
          vm,
          scope,
          host,
          hooks,
          "Invalid Headers init sequence item",
        ));
      };
      if scope.heap().object_prototype(entry_obj)? != Some(array_proto) {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          hooks,
          "Invalid Headers init sequence item",
        ));
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
  let max_header_bytes = with_env_state(env_id, scope.heap(), |state| {
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

  let result: std::result::Result<(), WebFetchError> = with_env_state_mut(env_id, scope.heap(), |state| {
    let headers = get_headers_mut(state, kind, owner)?;
    Ok(headers.append(&name, &value))
  })?;
  result.map_err(|err| map_web_fetch_error_to_throw(vm, scope, &mut *host, host_hooks, err))?;

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
  let max_header_bytes = with_env_state(env_id, scope.heap(), |state| {
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

  let result: std::result::Result<(), WebFetchError> = with_env_state_mut(env_id, scope.heap(), |state| {
    let headers = get_headers_mut(state, kind, owner)?;
    Ok(headers.set(&name, &value))
  })?;
  result.map_err(|err| map_web_fetch_error_to_throw(vm, scope, &mut *host, host_hooks, err))?;

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
  let max_header_bytes = with_env_state(env_id, scope.heap(), |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.limits().max_total_header_bytes)
  })?;
  let name = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_NAME_TOO_LONG_ERROR,
  )?;

  let result: std::result::Result<(), WebFetchError> = with_env_state_mut(env_id, scope.heap(), |state| {
    let headers = get_headers_mut(state, kind, owner)?;
    Ok(headers.delete(&name))
  })?;
  result.map_err(|err| map_web_fetch_error_to_throw(vm, scope, &mut *host, host_hooks, err))?;

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
  let max_header_bytes = with_env_state(env_id, scope.heap(), |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.limits().max_total_header_bytes)
  })?;
  let name = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_NAME_TOO_LONG_ERROR,
  )?;
  let has_result: std::result::Result<bool, WebFetchError> = with_env_state(env_id, scope.heap(), |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.has(&name))
  })?;
  let has = has_result.map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
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
  let max_header_bytes = with_env_state(env_id, scope.heap(), |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.limits().max_total_header_bytes)
  })?;
  let name = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_header_bytes,
    FETCH_HEADER_NAME_TOO_LONG_ERROR,
  )?;
  let value_result: std::result::Result<Option<String>, WebFetchError> =
    with_env_state(env_id, scope.heap(), |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.get(&name))
  })?;
  let value =
    value_result.map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
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
  let values = with_env_state(env_id, scope.heap(), |state| {
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

  let pairs = with_env_state(env_id, scope.heap(), |state| {
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
      &[
        Value::String(value_s),
        Value::String(name_s),
        Value::Object(headers_obj),
      ],
    )?;
  }

  Ok(Value::Undefined)
}

fn headers_iter_proto_from_callee(
  scope: &Scope<'_>,
  callee: GcObject,
) -> Result<GcObject, VmError> {
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
  let pairs = with_env_state(env_id, scope.heap(), |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.sort_and_combine())
  })?;
  let iter_id = with_env_state_mut(env_id, scope.heap(), |state| {
    let id = state.alloc_id();
    state
      .headers_iterators
      .insert(id, HeadersIteratorState { pairs, index: 0 });
    Ok(id)
  })?;

  let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: FETCH_HEADERS_ITERATOR_HOST_TAG,
      b: 0,
    },
  )?;
  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    HEADERS_ITER_ID_KEY,
    Value::Number(iter_id as f64),
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    HEADERS_ITER_KIND_KEY,
    Value::Number(HEADERS_ITER_KIND_ENTRIES as f64),
    false,
  )?;
  set_data_prop(scope, obj, HEADERS_ITER_DONE_KEY, Value::Bool(false), true)?;

  with_env_state_mut(env_id, scope.heap(), |state| {
    state
      .headers_iterators_wrappers
      .insert(iter_id, WeakGcObject::from(obj));
    Ok(())
  })?;
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
  let pairs = with_env_state(env_id, scope.heap(), |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.sort_and_combine())
  })?;
  let iter_id = with_env_state_mut(env_id, scope.heap(), |state| {
    let id = state.alloc_id();
    state
      .headers_iterators
      .insert(id, HeadersIteratorState { pairs, index: 0 });
    Ok(id)
  })?;

  let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: FETCH_HEADERS_ITERATOR_HOST_TAG,
      b: 0,
    },
  )?;
  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    HEADERS_ITER_ID_KEY,
    Value::Number(iter_id as f64),
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    HEADERS_ITER_KIND_KEY,
    Value::Number(HEADERS_ITER_KIND_KEYS as f64),
    false,
  )?;
  set_data_prop(scope, obj, HEADERS_ITER_DONE_KEY, Value::Bool(false), true)?;

  with_env_state_mut(env_id, scope.heap(), |state| {
    state
      .headers_iterators_wrappers
      .insert(iter_id, WeakGcObject::from(obj));
    Ok(())
  })?;
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
  let pairs = with_env_state(env_id, scope.heap(), |state| {
    let headers = get_headers_ref(state, kind, owner)?;
    Ok(headers.sort_and_combine())
  })?;
  let iter_id = with_env_state_mut(env_id, scope.heap(), |state| {
    let id = state.alloc_id();
    state
      .headers_iterators
      .insert(id, HeadersIteratorState { pairs, index: 0 });
    Ok(id)
  })?;

  let obj = scope.alloc_object_with_prototype(Some(iter_proto))?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: FETCH_HEADERS_ITERATOR_HOST_TAG,
      b: 0,
    },
  )?;
  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    HEADERS_ITER_ID_KEY,
    Value::Number(iter_id as f64),
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    HEADERS_ITER_KIND_KEY,
    Value::Number(HEADERS_ITER_KIND_VALUES as f64),
    false,
  )?;
  set_data_prop(scope, obj, HEADERS_ITER_DONE_KEY, Value::Bool(false), true)?;

  with_env_state_mut(env_id, scope.heap(), |state| {
    state
      .headers_iterators_wrappers
      .insert(iter_id, WeakGcObject::from(obj));
    Ok(())
  })?;
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
  let Some(slots) = scope.heap().object_host_slots(iter_obj)? else {
    return Err(VmError::TypeError("Headers iterator: illegal invocation"));
  };
  if slots.a != FETCH_HEADERS_ITERATOR_HOST_TAG {
    return Err(VmError::TypeError("Headers iterator: illegal invocation"));
  }

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
  let env_id = number_to_u64(env_val)
    .map_err(|_| VmError::TypeError("Headers iterator: illegal invocation"))?;
  let iter_id = number_to_u64(iter_val)
    .map_err(|_| VmError::TypeError("Headers iterator: illegal invocation"))?;
  let kind_u64 = number_to_u64(kind_val)
    .map_err(|_| VmError::TypeError("Headers iterator: illegal invocation"))?;
  let kind: u8 = kind_u64
    .try_into()
    .map_err(|_| VmError::TypeError("Headers iterator: illegal invocation"))?;

  let next_pair: Option<(String, String)> = with_env_state_mut(env_id, scope.heap(), |state| {
    let iter = state
      .headers_iterators
      .get_mut(&iter_id)
      .ok_or(VmError::TypeError(
        "Headers iterator: invalid backing iterator",
      ))?;
    if iter.index >= iter.pairs.len() {
      state.headers_iterators.remove(&iter_id);
      state.headers_iterators_wrappers.remove(&iter_id);
      Ok(None)
    } else {
      let pair = iter
        .pairs
        .get(iter.index)
        .cloned()
        .ok_or(VmError::InvariantViolation(
          "Headers iterator index out of bounds",
        ))?;
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
    set_data_prop(
      scope,
      iter_obj,
      HEADERS_ITER_DONE_KEY,
      Value::Bool(true),
      true,
    )?;

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
  let limits = with_env_state(env_id, scope.heap(), |state| Ok(state.env.limits.clone()))?;

  let mut core = CoreHeaders::new_with_guard_and_limits(HeadersGuard::None, &limits);
  if let Some(init) = args.get(0).copied() {
    // Fill before installing into the env state so errors don't leave partial state behind.
    fill_headers_from_init(vm, scope, &mut *host, host_hooks, env_id, &mut core, init)?;
  }

  let headers_id = with_env_state_mut(env_id, scope.heap(), |state| {
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
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: FETCH_HEADERS_HOST_TAG,
      b: 0,
    },
  )?;

  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    HEADERS_KIND_KEY,
    Value::Number(HEADERS_KIND_OWNED as f64),
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    HEADERS_OWNER_KEY,
    Value::Number(headers_id as f64),
    false,
  )?;

  with_env_state_mut(env_id, scope.heap(), |state| {
    state
      .owned_headers_wrappers
      .insert(headers_id, WeakGcObject::from(obj));
    Ok(())
  })?;

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

fn push_bytes_limited(
  out: &mut Vec<u8>,
  bytes: &[u8],
  max_len: usize,
  too_long_error: &'static str,
) -> Result<(), VmError> {
  let next_len = out
    .len()
    .checked_add(bytes.len())
    .ok_or(VmError::OutOfMemory)?;
  if next_len > max_len {
    return Err(VmError::TypeError(too_long_error));
  }
  out
    .try_reserve_exact(bytes.len())
    .map_err(|_| VmError::OutOfMemory)?;
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
  essence
    .bytes()
    .map(|b| (b as char).to_ascii_lowercase())
    .collect()
}

/// Encode a `FormData` entry list into a `multipart/form-data` body.
///
/// This is shared between `fetch()` and `XMLHttpRequest.send()` so both use identical multipart
/// escaping and boundary framing.
pub(crate) fn encode_form_data_as_multipart(
  entries: &[window_form_data::FormDataEntry],
  boundary: &str,
  max_len: usize,
  too_long_error: &'static str,
) -> Result<Vec<u8>, VmError> {
  let mut out = Vec::<u8>::new();

  for entry in entries {
    push_bytes_limited(&mut out, b"--", max_len, too_long_error)?;
    push_bytes_limited(&mut out, boundary.as_bytes(), max_len, too_long_error)?;
    push_bytes_limited(&mut out, b"\r\n", max_len, too_long_error)?;

    let escaped_name = escape_multipart_quoted_string_value(&entry.name);
    match &entry.value {
      window_form_data::FormDataValue::String(value) => {
        let header = format!("Content-Disposition: form-data; name=\"{escaped_name}\"\r\n\r\n");
        push_bytes_limited(&mut out, header.as_bytes(), max_len, too_long_error)?;
        push_bytes_limited(&mut out, value.as_bytes(), max_len, too_long_error)?;
        push_bytes_limited(&mut out, b"\r\n", max_len, too_long_error)?;
      }
      window_form_data::FormDataValue::File { data, filename, .. } => {
        let escaped_filename = escape_multipart_quoted_string_value(filename);
        let header = format!(
          "Content-Disposition: form-data; name=\"{escaped_name}\"; filename=\"{escaped_filename}\"\r\n"
        );
        push_bytes_limited(&mut out, header.as_bytes(), max_len, too_long_error)?;
        if !data.r#type.is_empty() {
          let content_type = format!("Content-Type: {}\r\n", data.r#type);
          push_bytes_limited(&mut out, content_type.as_bytes(), max_len, too_long_error)?;
        }
        push_bytes_limited(&mut out, b"\r\n", max_len, too_long_error)?;
        push_bytes_limited(&mut out, &data.bytes, max_len, too_long_error)?;
        push_bytes_limited(&mut out, b"\r\n", max_len, too_long_error)?;
      }
    }
  }

  push_bytes_limited(&mut out, b"--", max_len, too_long_error)?;
  push_bytes_limited(&mut out, boundary.as_bytes(), max_len, too_long_error)?;
  push_bytes_limited(&mut out, b"--\r\n", max_len, too_long_error)?;

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
  let Some(stripped) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
    return trimmed.to_string();
  };
  unescape_multipart_quoted_string_value(stripped)
}

fn extract_multipart_boundary(content_type: &str) -> Option<String> {
  for part in content_type.split(';').skip(1) {
    let part = part.trim();
    let Some(rest) = part
      .strip_prefix("boundary=")
      .or_else(|| part.strip_prefix("Boundary="))
    else {
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

fn parse_content_disposition_form_data(
  value: &str,
) -> Result<(String, Option<String>), &'static str> {
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
  file_last_modified_ms: i64,
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

    let headers_end = find_subslice(bytes, b"\r\n\r\n", pos)
      .ok_or("multipart/form-data headers missing terminator")?;
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

    let delimiter_pos =
      find_subslice(bytes, &delimiter, pos).ok_or("multipart/form-data part missing boundary")?;
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
        window_form_data::FormDataValue::File {
          data: window_blob::BlobData {
            bytes: part_bytes.to_vec(),
            r#type,
          },
          filename,
          last_modified: file_last_modified_ms,
        }
      }
      None => {
        window_form_data::FormDataValue::String(String::from_utf8_lossy(part_bytes).into_owned())
      }
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
  file_last_modified_ms: i64,
) -> Result<Vec<window_form_data::FormDataEntry>, &'static str> {
  let content_type = content_type.ok_or("Body.formData requires a Content-Type header")?;
  let essence = normalize_content_type_for_blob(content_type);
  match essence.as_str() {
    "application/x-www-form-urlencoded" => Ok(parse_urlencoded_form_data(bytes)),
    "multipart/form-data" => {
      let boundary = extract_multipart_boundary(content_type).ok_or("multipart/form-data missing boundary")?;
      parse_multipart_form_data(bytes, &boundary, file_last_modified_ms)
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
  body_stream_out: &mut Option<GcObject>,
  cache_out: &mut String,
  integrity_out: &mut String,
  keepalive_out: &mut bool,
  init: Value,
) -> Result<bool, VmError> {
  if matches!(init, Value::Undefined | Value::Null) {
    if cache_out.as_str() == "only-if-cached" && request.mode != RequestMode::SameOrigin {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "Request.cache \"only-if-cached\" requires Request.mode \"same-origin\"",
      ));
    }
    return Ok(false);
  }

  let Value::Object(init_obj) = init else {
    return Err(VmError::TypeError("Request init must be an object"));
  };

  *body_stream_out = None;
  let mut init_body_provided = false;

  // Fetch spec: `RequestInit.window` is `any` but can only be set to null.
  // If present (i.e. not `undefined`) and not `null`, Request construction must throw.
  let window_key = alloc_key(scope, "window")?;
  let window_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, window_key)?;
  if !matches!(window_val, Value::Undefined) && !matches!(window_val, Value::Null) {
    return Err(throw_type_error(
      vm,
      scope,
      host,
      host_hooks,
      "RequestInit.window must be null",
    ));
  }

  // `mode` must be applied before headers so the correct guard is enforced when filling.
  let mode_key = alloc_key(scope, "mode")?;
  let mode_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, mode_key)?;
  let mut mode_changed = false;
  if !matches!(mode_val, Value::Undefined | Value::Null) {
    let mode_s = to_rust_string_limited(scope.heap_mut(), mode_val, 64, FETCH_MODE_TOO_LONG_ERROR)?;
    let mode = match mode_s.as_str() {
      "navigate" => {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "Request.mode \"navigate\" is not allowed",
        ));
      }
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

  // RequestInit `cache`/`integrity`/`keepalive` are wrapper-only attributes (the core request type
  // does not currently represent them). They still need to be parsed and validated to match spec
  // behavior and WPT expectations.
  let cache_key = alloc_key(scope, "cache")?;
  let cache_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, cache_key)?;
  if !matches!(cache_val, Value::Undefined) {
    let cache_s = to_rust_string_limited(scope.heap_mut(), cache_val, 64, FETCH_CACHE_TOO_LONG_ERROR)?;
    match cache_s.as_str() {
      "default" | "no-store" | "reload" | "no-cache" | "force-cache" | "only-if-cached" => {
        *cache_out = cache_s;
      }
      _ => {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "Request.cache must be \"default\", \"no-store\", \"reload\", \"no-cache\", \"force-cache\", or \"only-if-cached\"",
        ));
      }
    }
  }

  if cache_out.as_str() == "only-if-cached" && request.mode != RequestMode::SameOrigin {
    return Err(throw_type_error(
      vm,
      scope,
      host,
      host_hooks,
      "Request.cache \"only-if-cached\" requires Request.mode \"same-origin\"",
    ));
  }

  let integrity_key = alloc_key(scope, "integrity")?;
  let integrity_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, integrity_key)?;
  if !matches!(integrity_val, Value::Undefined) {
    *integrity_out = to_rust_string_limited(
      scope.heap_mut(),
      integrity_val,
      limits.max_url_bytes,
      FETCH_INTEGRITY_TOO_LONG_ERROR,
    )?;
  }

  let keepalive_key = alloc_key(scope, "keepalive")?;
  let keepalive_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, keepalive_key)?;
  if !matches!(keepalive_val, Value::Undefined) {
    *keepalive_out = scope.heap().to_boolean(keepalive_val)?;
  }

  // `headers` replaces the existing header list.
  let headers_key = alloc_key(scope, "headers")?;
  let headers_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, headers_key)?;
  if !matches!(headers_val, Value::Undefined | Value::Null) {
    let mut headers =
      CoreHeaders::new_with_guard_and_limits(request.headers.guard(), request.headers.limits());
    fill_headers_from_init(
      vm,
      scope,
      host,
      host_hooks,
      env_id,
      &mut headers,
      headers_val,
    )?;
    request.headers = headers;
  } else if mode_changed {
    // If mode changed (e.g. "cors" -> "no-cors"), re-apply the header list so any now-forbidden
    // headers are removed deterministically.
    let existing = request.headers.raw_pairs();
    let mut headers =
      CoreHeaders::new_with_guard_and_limits(request.headers.guard(), request.headers.limits());
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
    let redirect_s = to_rust_string_limited(
      scope.heap_mut(),
      redirect_val,
      64,
      FETCH_REDIRECT_TOO_LONG_ERROR,
    )?;
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
    let raw = to_rust_string_limited(
      scope.heap_mut(),
      referrer_val,
      limits.max_url_bytes,
      FETCH_REFERRER_TOO_LONG_ERROR,
    )?;

    // https://fetch.spec.whatwg.org/#dom-requestinit-referrer
    // FastRender internal referrer state is stored as:
    // - "" (empty) => "client" referrer state (use execution context default)
    // - "no-referrer" => explicit omission
    // - any other string => absolute URL
    if raw.is_empty() {
      request.referrer = "no-referrer".to_string();
    } else {
      let base_url = current_document_base_url(vm, scope.heap(), env_id)?;
      let resolved = resolve_url(&raw, base_url.as_deref())
        .map_err(|err| throw_type_error(vm, scope, host, host_hooks, &err.to_string()))?;

      let parsed = ::url::Url::parse(&resolved)
        .map_err(|err| throw_type_error(vm, scope, host, host_hooks, &err.to_string()))?;

      if parsed.scheme() == "about" && parsed.path() == "client" {
        // "about:client" maps to the internal "client" referrer state.
        request.referrer = String::new();
      } else {
        let same_origin = match (origin_from_url(&request.url), origin_from_url(&resolved)) {
          (Some(request_origin), Some(referrer_origin)) => request_origin.same_origin(&referrer_origin),
          _ => false,
        };

        if !same_origin {
          // Cross-origin referrers are not stored; fall back to the internal "client" state.
          request.referrer = String::new();
        } else {
          request.referrer = resolved;
        }
      }
    }
  }

  let referrer_policy_key = alloc_key(scope, "referrerPolicy")?;
  let referrer_policy_val =
    vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, referrer_policy_key)?;
  if !matches!(referrer_policy_val, Value::Undefined | Value::Null) {
    let policy_s = to_rust_string_limited(
      scope.heap_mut(),
      referrer_policy_val,
      64,
      FETCH_REFERRER_POLICY_TOO_LONG_ERROR,
    )?;
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
  let credentials_val =
    vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, credentials_key)?;
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
    init_body_provided = true;
    let max_body_bytes = request.headers.limits().max_request_body_bytes;
    let mut inferred_content_type: Option<String> = None;

    match body_val {
      Value::Object(obj) => {
        if is_readable_stream_like_object(vm, scope, host, host_hooks, obj)? {
          // Track disturbed state for external body streams (Fetch's `bodyUsed`).
          ensure_body_stream_wrappers_installed(vm, scope, host, host_hooks, env_id, obj)?;
          if readable_stream_is_locked(vm, scope, host, host_hooks, obj)? {
            return Err(throw_type_error(
              vm,
              scope,
              host,
              host_hooks,
              "Request body is locked",
            ));
          }
          if body_stream_is_used(scope, obj)? {
            return Err(throw_type_error(
              vm,
              scope,
              host,
              host_hooks,
              "Request body is already used",
            ));
          }
          // The core fetch layer only supports in-memory bodies. When a ReadableStream is
          // provided, we buffer it (bounded by `WebFetchLimits`) before dispatching the request;
          // true streaming uploads / `duplex` are not supported.
          request.body = None;
          *body_stream_out = Some(obj);
        } else {
          let bytes: Vec<u8> = if scope.heap().is_array_buffer_object(obj) {
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
          } else if scope.heap().is_typed_array_object(obj) {
            // BufferSource (ArrayBufferView): copy the visible bytes.
            let byte_length = scope.heap().typed_array_byte_length(obj)?;
            if byte_length > max_body_bytes {
              return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
            }
            let byte_offset = scope.heap().typed_array_byte_offset(obj)?;
            let buffer = scope.heap().typed_array_buffer(obj)?;
            let data = scope.heap().array_buffer_data(buffer)?;
            let end = byte_offset
              .checked_add(byte_length)
              .ok_or(VmError::TypeError("TypedArray byte offset overflow"))?;
            let slice = data
              .get(byte_offset..end)
              .ok_or(VmError::TypeError("TypedArray view out of bounds"))?;
            slice.to_vec()
          } else if scope.heap().is_data_view_object(obj) {
            // BufferSource (DataView): copy the visible bytes.
            let byte_length = scope.heap().data_view_byte_length(obj)?;
            if byte_length > max_body_bytes {
              return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
            }
            let byte_offset = scope.heap().data_view_byte_offset(obj)?;
            let buffer = scope.heap().data_view_buffer(obj)?;
            let data = scope.heap().array_buffer_data(buffer)?;
            let end = byte_offset
              .checked_add(byte_length)
              .ok_or(VmError::TypeError("DataView byte offset overflow"))?;
            let slice = data
              .get(byte_offset..end)
              .ok_or(VmError::TypeError("DataView view out of bounds"))?;
            slice.to_vec()
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
            let boundary_id = with_env_state_mut(env_id, scope.heap(), |state| {
              let id = state.multipart_boundary_counter;
              state.multipart_boundary_counter = state.multipart_boundary_counter.saturating_add(1);
              Ok(id)
            })?;
            let boundary = format!("----fastrenderformdata{boundary_id}");
            let multipart = encode_form_data_as_multipart(
              &entries,
              &boundary,
              max_body_bytes,
              FETCH_BODY_TOO_LONG_ERROR,
            )?;
            inferred_content_type = Some(format!("multipart/form-data; boundary={boundary}"));
            multipart
          } else {
            inferred_content_type = Some("text/plain;charset=UTF-8".to_string());
            let s = scope.to_string(vm, host, host_hooks, body_val)?;
            js_string_to_rust_string_limited(
              scope.heap(),
              s,
              max_body_bytes,
              FETCH_BODY_TOO_LONG_ERROR,
            )?
            .into_bytes()
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
      }
      other => {
        let inferred_content_type = "text/plain;charset=UTF-8";
        let s = scope.to_string(vm, host, host_hooks, other)?;
        let bytes = js_string_to_rust_string_limited(
          scope.heap(),
          s,
          max_body_bytes,
          FETCH_BODY_TOO_LONG_ERROR,
        )?
        .into_bytes();

        let has_content_type = request
          .headers
          .has("Content-Type")
          .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
        if !has_content_type {
          request
            .headers
            .set("Content-Type", inferred_content_type)
            .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
        }

        let body = Body::new_with_limits(bytes, request.headers.limits())
          .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, host_hooks, err))?;
        request.body = Some(body);
      }
    };
  }

  // Fetch invariants.
  if request.method.eq_ignore_ascii_case("GET") || request.method.eq_ignore_ascii_case("HEAD") {
    if request.body.is_some() || body_stream_out.is_some() {
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

  Ok(init_body_provided)
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

/// Create a dependent AbortSignal for `Request.signal`.
///
/// Fetch's `Request` interface requires `request.signal` to always be an `AbortSignal` and to be a
/// *new* dependent signal derived from a list of source signals (including the empty list).
///
/// When AbortSignal bindings are installed, this uses `AbortSignal.any([...])` which closely
/// matches the spec's "creating a dependent abort signal from signals" primitive.
///
/// If AbortSignal bindings are unavailable (e.g. in minimal unit tests that only install fetch),
/// this falls back to creating a deterministic plain object with `{ aborted, reason }` snapshot
/// properties so `Request.signal` is never `null`. This fallback does **not** attempt to fully
/// emulate AbortSignal semantics (event dispatch, abort propagation, etc).
fn create_request_signal(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  env_id: u64,
  sources: &[GcObject],
) -> Result<Value, VmError> {
  let heap = scope.heap();
  let global_obj = with_env_state(env_id, heap, |state| Ok(state.global.upgrade(heap)))?;

  if let Some(global_obj) = global_obj {
    scope.push_root(Value::Object(global_obj))?;

    let abort_signal_ctor_val = {
      let abort_signal_key = alloc_key(scope, "AbortSignal")?;
      vm.get_with_host_and_hooks(host, scope, host_hooks, global_obj, abort_signal_key)?
    };

    if let Value::Object(abort_signal_ctor) = abort_signal_ctor_val {
      scope.push_root(Value::Object(abort_signal_ctor))?;

      let any_fn = {
        let any_key = alloc_key(scope, "any")?;
        vm.get_with_host_and_hooks(host, scope, host_hooks, abort_signal_ctor, any_key)?
      };

      if scope.heap().is_callable(any_fn).unwrap_or(false) {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

        // Build `[...]` as an actual Array so `AbortSignal.any` sees a well-formed iterable.
        let arr = scope.alloc_array(sources.len())?;
        scope
          .heap_mut()
          .object_set_prototype(arr, Some(intr.array_prototype()))?;
        scope.push_root(Value::Object(arr))?;

        for (idx, source) in sources.iter().enumerate() {
          // Root the element while allocating the property key: `alloc_key` can trigger GC.
          scope.push_root(Value::Object(*source))?;
          let key = alloc_key(scope, &idx.to_string())?;
          scope.define_property(arr, key, data_desc(Value::Object(*source), true))?;
        }

        // AbortSignal.any([...]) -> AbortSignal
        let signal_val = vm.call_with_host_and_hooks(
          host,
          scope,
          host_hooks,
          any_fn,
          Value::Object(abort_signal_ctor),
          &[Value::Object(arr)],
        )?;
        if matches!(signal_val, Value::Object(_)) {
          return Ok(signal_val);
        }
      }
    }
  }

  // Deterministic fallback: create a plain object with `{ aborted, reason }` based on the current
  // state of the provided signals.
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(obj))?;

  let aborted_key = alloc_key(scope, "aborted")?;
  let reason_key = alloc_key(scope, "reason")?;

  let mut aborted = false;
  let mut reason = Value::Undefined;

  for source in sources {
    // Root `source` while reading its properties in case allocation triggers GC.
    let mut check_scope = scope.reborrow();
    check_scope.push_root(Value::Object(*source))?;

    let aborted_val =
      vm.get_with_host_and_hooks(host, &mut check_scope, host_hooks, *source, aborted_key)?;
    if check_scope.heap().to_boolean(aborted_val)? {
      aborted = true;
      reason =
        vm.get_with_host_and_hooks(host, &mut check_scope, host_hooks, *source, reason_key)?;
      break;
    }
  }

  // Root `reason` while defining `aborted` in case `set_data_prop` triggers GC.
  scope.push_root(reason)?;
  set_data_prop(scope, obj, "aborted", Value::Bool(aborted), /* writable */ false)?;
  set_data_prop(scope, obj, "reason", reason, /* writable */ false)?;
  Ok(Value::Object(obj))
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
  let limits = with_env_state(env_id, scope.heap(), |state| Ok(state.env.limits.clone()))?;
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let init = args.get(1).copied().unwrap_or(Value::Undefined);

  let input_request_info = request_info_from_value(scope, input);
  let input_request_obj = match (input_request_info, input) {
    (Some(_), Value::Object(obj)) => Some(obj),
    _ => None,
  };

  let mut request = if let Some((other_env_id, other_request_id)) = input_request_info {
    let cloned: Option<CoreRequest> = with_env_state(other_env_id, scope.heap(), |state| {
      let req = state
        .requests
        .get(&other_request_id)
        .ok_or(VmError::TypeError("Request: invalid backing request"))?;
      if req.body.as_ref().map_or(false, |b| b.body_used()) {
        Ok(None)
      } else {
        Ok(Some(req.clone()))
      }
    })?;
    match cloned {
      Some(req) => req,
      None => {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "Request body is already used",
        ));
      }
    }
  } else {
    let url = to_rust_string_limited(
      scope.heap_mut(),
      input,
      limits.max_url_bytes,
      FETCH_URL_TOO_LONG_ERROR,
    )?;
    let base_url = current_document_base_url(vm, scope.heap(), env_id)?;
    let url = resolve_url(&url, base_url.as_deref())
      .map_err(|err| throw_type_error(vm, scope, host, host_hooks, &err.to_string()))?;
    CoreRequest::new_with_limits("GET", url, &limits)
  };

  // Associated AbortSignal.
  //
  // Fetch requires `request.signal` to always be a new dependent signal:
  // - `init.signal` (if present, even if it is `null`) takes precedence
  // - otherwise, inherit from the input request's signal (if any)
  // - otherwise, create a fresh non-aborted signal.
  let mut signal_sources: Vec<GcObject> = Vec::new();
  let mut init_specified_signal = false;

  // Wrapper-only Request attributes that are not stored in the core request type.
  let mut cache = "default".to_string();
  let mut integrity = String::new();
  let mut keepalive = false;
  if let Some(input_obj) = input_request_obj {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(input_obj))?;

    let cache_val = get_data_prop(&mut scope, input_obj, "cache")?;
    if !matches!(cache_val, Value::Undefined) {
      cache = to_rust_string_limited(scope.heap_mut(), cache_val, 64, FETCH_CACHE_TOO_LONG_ERROR)?;
    }

    let integrity_val = get_data_prop(&mut scope, input_obj, "integrity")?;
    if !matches!(integrity_val, Value::Undefined) {
      integrity = to_rust_string_limited(
        scope.heap_mut(),
        integrity_val,
        limits.max_url_bytes,
        FETCH_INTEGRITY_TOO_LONG_ERROR,
      )?;
    }

    let keepalive_val = get_data_prop(&mut scope, input_obj, "keepalive")?;
    if !matches!(keepalive_val, Value::Undefined) {
      keepalive = scope.heap().to_boolean(keepalive_val)?;
    }
  }

  let mut body_stream: Option<GcObject> = None;
  let init_body_provided = apply_request_init(
    vm,
    scope,
    host,
    host_hooks,
    env_id,
    &limits,
    &mut request,
    &mut body_stream,
    &mut cache,
    &mut integrity,
    &mut keepalive,
    init,
  )?;

  // Request constructor invariant: user-constructed Requests must never have mode "navigate",
  // including when inheriting from an input Request.
  if request.mode == RequestMode::Navigate {
    return Err(throw_type_error(
      vm,
      scope,
      host,
      host_hooks,
      "Request.mode \"navigate\" is not allowed",
    ));
  }

  // Enforce invariants even when `init` is omitted (e.g. `new Request(existingRequest)`).
  request.method =
    normalize_and_validate_method(vm, scope, host, host_hooks, request.method.as_str())?;
  if request.method.eq_ignore_ascii_case("GET") || request.method.eq_ignore_ascii_case("HEAD") {
    if request.body.is_some() || body_stream.is_some() {
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

  if !init_body_provided {
    if let Some(input_obj) = input_request_obj {
      if request_wrapper_cached_body_stream_is_locked(vm, scope, host, host_hooks, input_obj)? {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "Request body is locked",
        ));
      }

      // If the input Request was backed by an external stream (i.e. no in-memory body), clone it by
      // teeing the stream (or throw if tee is unavailable).
      if request.body.is_none() && body_stream.is_none() {
        let cached = get_data_prop(scope, input_obj, REQUEST_BODY_STREAM_KEY)?;
        if let Value::Object(input_stream) = cached {
          if body_stream_is_used(scope, input_stream)? {
            return Err(throw_type_error(
              vm,
              scope,
              host,
              host_hooks,
              "Request body is already used",
            ));
          }
          let (s1, s2) = readable_stream_tee(vm, scope, host, host_hooks, input_stream)?;
          ensure_body_stream_wrappers_installed(vm, scope, host, host_hooks, env_id, s1)?;
          ensure_body_stream_wrappers_installed(vm, scope, host, host_hooks, env_id, s2)?;
          set_data_prop(
            scope,
            input_obj,
            REQUEST_BODY_STREAM_KEY,
            Value::Object(s1),
            false,
          )?;
          body_stream = Some(s2);
        }
      }
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
      return Err(VmError::InvariantViolation(
        "Request init must be an object",
      ));
    };
    let signal_key = alloc_key(scope, "signal")?;
    let signal_val = vm.get_with_host_and_hooks(host, scope, host_hooks, init_obj, signal_key)?;
    if !matches!(signal_val, Value::Undefined) {
      init_specified_signal = true;
      match signal_val {
        Value::Undefined | Value::Null => {}
        Value::Object(obj) => signal_sources.push(obj),
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
      if let Value::Object(obj) = inherited {
        signal_sources.push(obj);
      }
    }
  }

  let signal = create_request_signal(vm, scope, host, host_hooks, env_id, &signal_sources)?;
  // Root the newly created signal while we allocate the wrapper object below.
  scope.push_root(signal)?;

  let request_id = with_env_state_mut(env_id, scope.heap(), |state| {
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

  let cache_s = scope.alloc_string(&cache)?;
  set_data_prop(scope, obj, "cache", Value::String(cache_s), false)?;
  let integrity_s = scope.alloc_string(&integrity)?;
  set_data_prop(scope, obj, "integrity", Value::String(integrity_s), false)?;
  set_data_prop(scope, obj, "keepalive", Value::Bool(keepalive), false)?;

  set_data_prop(
    scope,
    obj,
    "signal",
    signal,
    /* writable */ false,
  )?;

  if let Some(stream_obj) = body_stream {
    set_data_prop(
      scope,
      obj,
      REQUEST_BODY_STREAM_KEY,
      Value::Object(stream_obj),
      false,
    )?;
  }
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
  let cache = get_data_prop(scope, original_obj, "cache")?;
  let integrity = get_data_prop(scope, original_obj, "integrity")?;
  let keepalive = get_data_prop(scope, original_obj, "keepalive")?;
  let mut signal_sources: Vec<GcObject> = Vec::new();
  if let Value::Object(obj) = get_data_prop(scope, original_obj, "signal")? {
    signal_sources.push(obj);
  }
  let signal = create_request_signal(vm, scope, host, host_hooks, env_id, &signal_sources)?;
  // Root the newly created signal while we allocate the wrapper object below.
  scope.push_root(signal)?;
  if request_wrapper_cached_body_stream_is_locked(vm, scope, host, host_hooks, original_obj)? {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Request body is locked",
    ));
  }

  if request_body_stream_locked(vm, env_id, request_id, scope.heap_mut())? {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Request body is locked",
    ));
  }

  let (cloned, has_core_body) = with_env_state(env_id, scope.heap(), |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    let has_core_body = req.body.is_some();
    if req.body.as_ref().map_or(false, |b| b.body_used()) {
      Ok((None, has_core_body))
    } else {
      Ok((Some(req.clone()), has_core_body))
    }
  })?;
  let Some(cloned) = cloned else {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Request body is already used",
    ));
  };

  // Streaming body: tee the stream so the clone and original can be consumed independently.
  let mut cloned_body_stream: Option<GcObject> = None;
  if !has_core_body {
    let cached = get_data_prop(scope, original_obj, REQUEST_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if body_stream_is_used(scope, stream_obj)? {
        return Err(throw_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Request body is already used",
        ));
      }
      let (s1, s2) = readable_stream_tee(vm, scope, host, host_hooks, stream_obj)?;
      ensure_body_stream_wrappers_installed(vm, scope, host, host_hooks, env_id, s1)?;
      ensure_body_stream_wrappers_installed(vm, scope, host, host_hooks, env_id, s2)?;
      set_data_prop(
        scope,
        original_obj,
        REQUEST_BODY_STREAM_KEY,
        Value::Object(s1),
        false,
      )?;
      cloned_body_stream = Some(s2);
    }
  }

  let new_request_id = with_env_state_mut(env_id, scope.heap(), |state| {
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

  if !matches!(cache, Value::Undefined) {
    set_data_prop(scope, obj, "cache", cache, false)?;
  }
  if !matches!(integrity, Value::Undefined) {
    set_data_prop(scope, obj, "integrity", integrity, false)?;
  }
  if !matches!(keepalive, Value::Undefined) {
    set_data_prop(scope, obj, "keepalive", keepalive, false)?;
  }

  set_data_prop(scope, obj, "signal", signal, /* writable */ false)?;

  if let Some(stream_obj) = cloned_body_stream {
    set_data_prop(
      scope,
      obj,
      REQUEST_BODY_STREAM_KEY,
      Value::Object(stream_obj),
      false,
    )?;
  }
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
  let Value::Object(req_obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(req_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    Ok(req.body.is_some())
  })?;

  if !has_core_body {
    let cached = get_data_prop(scope, req_obj, REQUEST_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Request body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      let max_bytes = with_env_state(env_id, scope.heap(), |state| {
        let req = state
          .requests
          .get(&request_id)
          .ok_or(VmError::TypeError("Request: invalid backing request"))?;
        Ok(req.headers.limits().max_request_body_bytes)
      })?;
      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::Text,
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if request_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, req_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let result: std::result::Result<String, WebFetchError> =
    with_env_state_mut(env_id, scope.heap(), |state| {
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
  let Value::Object(req_obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(req_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    Ok(req.body.is_some())
  })?;
  if !has_core_body {
    let cached = get_data_prop(scope, req_obj, REQUEST_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Request body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      let max_bytes = with_env_state(env_id, scope.heap(), |state| {
        let req = state
          .requests
          .get(&request_id)
          .ok_or(VmError::TypeError("Request: invalid backing request"))?;
        Ok(req.headers.limits().max_request_body_bytes)
      })?;
      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::ArrayBuffer,
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if request_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, req_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let result: std::result::Result<Vec<u8>, WebFetchError> =
    with_env_state_mut(env_id, scope.heap(), |state| {
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

fn request_bytes_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(req_obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(req_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    Ok(req.body.is_some())
  })?;
  if !has_core_body {
    let cached = get_data_prop(scope, req_obj, REQUEST_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Request body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      let max_bytes = with_env_state(env_id, scope.heap(), |state| {
        let req = state
          .requests
          .get(&request_id)
          .ok_or(VmError::TypeError("Request: invalid backing request"))?;
        Ok(req.headers.limits().max_request_body_bytes)
      })?;
      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::Bytes,
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if request_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, req_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let result: std::result::Result<Vec<u8>, WebFetchError> =
    with_env_state_mut(env_id, scope.heap(), |state| {
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
      let byte_len = bytes.len();
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      let view = scope.alloc_uint8_array(ab, 0, byte_len)?;
      scope
        .heap_mut()
        .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.resolve,
        Value::Undefined,
        &[Value::Object(view)],
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
  let Value::Object(req_obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(req_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    Ok(req.body.is_some())
  })?;
  if !has_core_body {
    let cached = get_data_prop(scope, req_obj, REQUEST_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Request body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }

      let (max_bytes, blob_type) = with_env_state(env_id, scope.heap(), |state| {
        let req = state
          .requests
          .get(&request_id)
          .ok_or(VmError::TypeError("Request: invalid backing request"))?;
        let max_bytes = req.headers.limits().max_request_body_bytes;
        let ct = req.headers.get("Content-Type");
        Ok((max_bytes, ct))
      })?;

      let blob_type = match blob_type {
        Ok(v) => v,
        Err(err) => {
          let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
          let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.reject,
            Value::Undefined,
            &[err_value],
          )?;
          return Ok(cap.promise);
        }
      }
        .as_deref()
        .map(normalize_content_type_for_blob)
        .unwrap_or_default();

      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::Blob { blob_type },
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if request_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, req_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let (bytes_result, content_type_result): (
    std::result::Result<Vec<u8>, WebFetchError>,
    std::result::Result<Option<String>, WebFetchError>,
  ) = with_env_state_mut(env_id, scope.heap(), |state| {
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
        let proto = window_blob::blob_prototype_for_realm(realm_id).ok_or(
          VmError::Unimplemented("Request.blob requires Blob to be installed"),
        )?;
        window_blob::create_blob_with_proto(
          vm,
          scope,
          callee,
          proto,
          window_blob::BlobData {
            bytes,
            r#type: blob_type,
          },
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
  let Value::Object(req_obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(req_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    Ok(req.body.is_some())
  })?;
  if !has_core_body {
    let cached = get_data_prop(scope, req_obj, REQUEST_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Request body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }

      let (max_bytes, content_type_result) = with_env_state(env_id, scope.heap(), |state| {
        let req = state
          .requests
          .get(&request_id)
          .ok_or(VmError::TypeError("Request: invalid backing request"))?;
        let max_bytes = req.headers.limits().max_request_body_bytes;
        let ct = req.headers.get("Content-Type");
        Ok((max_bytes, ct))
      })?;

      let content_type = match content_type_result {
        Ok(v) => v,
        Err(err) => {
          let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
          let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.reject,
            Value::Undefined,
            &[err_value],
          )?;
          return Ok(cap.promise);
        }
      };

      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::FormData { content_type },
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if request_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, req_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let (bytes_result, content_type_result): (
    std::result::Result<Vec<u8>, WebFetchError>,
    std::result::Result<Option<String>, WebFetchError>,
  ) = with_env_state_mut(env_id, scope.heap(), |state| {
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
      let file_last_modified_ms =
        if content_type
          .as_deref()
          .is_some_and(|ct| normalize_content_type_for_blob(ct) == "multipart/form-data")
        {
          time::date_now_ms(scope)?
        } else {
          0
        };
      let entries =
        parse_form_data_entries_from_body(content_type.as_deref(), &bytes, file_last_modified_ms);
      match entries {
        Ok(entries) => {
          let form_data_result =
            window_form_data::create_form_data_with_entries(vm, scope, callee, entries);
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
                VmError::TypeError(msg) => {
                  create_type_error(vm, scope, &mut *host, host_hooks, msg)?
                }
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
  let Value::Object(req_obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(req_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    Ok(req.body.is_some())
  })?;
  if !has_core_body {
    let cached = get_data_prop(scope, req_obj, REQUEST_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Request body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      let max_bytes = with_env_state(env_id, scope.heap(), |state| {
        let req = state
          .requests
          .get(&request_id)
          .ok_or(VmError::TypeError("Request: invalid backing request"))?;
        Ok(req.headers.limits().max_request_body_bytes)
      })?;
      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::Json,
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if request_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, req_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Request body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let parsed: std::result::Result<serde_json::Value, WebFetchError> =
    with_env_state_mut(env_id, scope.heap(), |state| {
      let req = state
        .requests
        .get_mut(&request_id)
        .ok_or(VmError::TypeError("Request: invalid backing request"))?;
      let parsed = match req.body.as_mut() {
        Some(body) => body.json(),
        None => {
          // Fetch: "consume body" on a null body yields an empty byte sequence.
          // JSON parsing then fails with a SyntaxError (rather than a TypeError).
          let mut body = Body::empty();
          body.json()
        }
      };
      Ok(parsed)
    })?;

  match parsed {
    Ok(value) => {
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
    Err(err) => match err {
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
  let Value::Object(req_obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(req_obj))?;
  let core_used = with_env_state(env_id, scope.heap(), |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    Ok(req.body.as_ref().map_or(false, |b| b.body_used()))
  })?;
  if core_used {
    return Ok(Value::Bool(true));
  }

  let cached = get_data_prop(scope, req_obj, REQUEST_BODY_STREAM_KEY)?;
  let Value::Object(stream_obj) = cached else {
    return Ok(Value::Bool(false));
  };
  Ok(Value::Bool(body_stream_is_used(scope, stream_obj)?))
}

fn request_body_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(req_obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(req_obj))?;

  // Cache per Request instance.
  let cached = get_data_prop(scope, req_obj, REQUEST_BODY_STREAM_KEY)?;
  if let Value::Object(stream_obj) = cached {
    return Ok(Value::Object(stream_obj));
  }

  let has_body = with_env_state(env_id, scope.heap(), |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    Ok(req.body.is_some())
  })?;

  if !has_body {
    return Ok(Value::Null);
  }

  // `window_streams::create_readable_byte_stream_lazy` needs a realm id. When this accessor is
  // invoked without an active `ExecutionContext` (e.g. Rust tests calling `vm.get` directly),
  // provide a temporary callee with a realm-id slot so streams can resolve per-realm state.
  let streams_callee = if vm.current_realm().is_some() {
    callee
  } else {
    let (realm_id, call_id) = with_env_state(env_id, scope.heap(), |state| {
      Ok((state.realm_id, state.promise_executor_call))
    })?;
    let name_s = scope.alloc_string("__fastrender_streams_realm")?;
    scope.push_root(Value::String(name_s))?;
    let token_fn = scope.alloc_native_function_with_slots(
      call_id,
      None,
      name_s,
      0,
      &[Value::Number(realm_id.to_raw() as f64)],
    )?;
    scope.push_root(Value::Object(token_fn))?;
    token_fn
  };

  let stream_obj = window_streams::create_readable_byte_stream_lazy(vm, scope, streams_callee, move || {
    with_env_state_mut_no_sweep(env_id, |state| {
      let req = state
        .requests
        .get_mut(&request_id)
        .ok_or(VmError::TypeError("Request: invalid backing request"))?;
      match req.body.as_mut() {
        None => Ok(Vec::new()),
        Some(body) => {
          if body.body_used() {
            Ok(Vec::new())
          } else {
            body
              .consume_bytes()
              .map_err(|_| VmError::TypeError("Request body is already used"))
          }
        }
      }
    })
  })?;
  scope.push_root(Value::Object(stream_obj))?;

  with_env_state_mut(env_id, scope.heap(), |state| {
    state
      .request_body_stream_wrappers
      .insert(request_id, WeakGcObject::from(stream_obj));
    Ok(())
  })?;

  set_data_prop(
    scope,
    req_obj,
    REQUEST_BODY_STREAM_KEY,
    Value::Object(stream_obj),
    false,
  )?;

  Ok(Value::Object(stream_obj))
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

  let response_id = with_env_state_mut(env_id, scope.heap(), |state| {
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

  let max_url_bytes = with_env_state(env_id, scope.heap(), |state| Ok(state.env.limits.max_url_bytes))?;
  let url_input = to_rust_string_limited(
    scope.heap_mut(),
    args.get(0).copied().unwrap_or(Value::Undefined),
    max_url_bytes,
    FETCH_URL_TOO_LONG_ERROR,
  )?;
  let base_url = current_document_base_url(vm, scope.heap(), env_id)?;
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

  let response_id = with_env_state_mut(env_id, scope.heap(), |state| {
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
  let limits = with_env_state(env_id, scope.heap(), |state| Ok(state.env.limits.clone()))?;

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
    let stringify_fn =
      vm.get_with_host_and_hooks(&mut *host, &mut call_scope, host_hooks, json, stringify_key)?;

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
      _ => {
        return Err(VmError::InvariantViolation(
          "JSON.stringify returned non-string",
        ))
      }
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
    let status_val =
      vm.get_with_host_and_hooks(&mut *host, scope, host_hooks, init_obj, status_key)?;
    if !matches!(status_val, Value::Undefined) {
      let n = scope.heap_mut().to_number(status_val)?;
      status = number_to_u16_wrapping(n);
    }
    let status_text_key = alloc_key(scope, "statusText")?;
    let st_val =
      vm.get_with_host_and_hooks(&mut *host, scope, host_hooks, init_obj, status_text_key)?;
    if !matches!(st_val, Value::Undefined) {
      status_text = to_rust_string_limited(
        scope.heap_mut(),
        st_val,
        limits.max_url_bytes,
        FETCH_STATUS_TEXT_TOO_LONG_ERROR,
      )?;
    }
    let headers_key = alloc_key(scope, "headers")?;
    let headers_val =
      vm.get_with_host_and_hooks(&mut *host, scope, host_hooks, init_obj, headers_key)?;
    if !matches!(headers_val, Value::Undefined | Value::Null) {
      fill_headers_from_init(
        vm,
        scope,
        &mut *host,
        host_hooks,
        env_id,
        &mut headers,
        headers_val,
      )?;
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

  let response_id = with_env_state_mut(env_id, scope.heap(), |state| {
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
  let Value::Object(resp_obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  let (env_id, response_id) = response_info_from_this(scope, Value::Object(resp_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    Ok(res.body.is_some())
  })?;

  if !has_core_body {
    let cached = get_data_prop(scope, resp_obj, RESPONSE_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Response body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      let max_bytes = with_env_state(env_id, scope.heap(), |state| {
        let res = state
          .responses
          .get(&response_id)
          .ok_or(VmError::TypeError("Response: invalid backing response"))?;
        Ok(res.headers.limits().max_response_body_bytes)
      })?;
      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::Text,
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if response_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, resp_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let result: std::result::Result<String, WebFetchError> = with_env_state_mut(env_id, scope.heap(), |state| {
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
  let Value::Object(resp_obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  let (env_id, response_id) = response_info_from_this(scope, Value::Object(resp_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    Ok(res.body.is_some())
  })?;

  if !has_core_body {
    let cached = get_data_prop(scope, resp_obj, RESPONSE_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Response body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      let max_bytes = with_env_state(env_id, scope.heap(), |state| {
        let res = state
          .responses
          .get(&response_id)
          .ok_or(VmError::TypeError("Response: invalid backing response"))?;
        Ok(res.headers.limits().max_response_body_bytes)
      })?;
      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::ArrayBuffer,
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if response_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, resp_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let result: std::result::Result<Vec<u8>, WebFetchError> = with_env_state_mut(env_id, scope.heap(), |state| {
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

fn response_bytes_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(resp_obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  let (env_id, response_id) = response_info_from_this(scope, Value::Object(resp_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    Ok(res.body.is_some())
  })?;

  if !has_core_body {
    let cached = get_data_prop(scope, resp_obj, RESPONSE_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Response body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      let max_bytes = with_env_state(env_id, scope.heap(), |state| {
        let res = state
          .responses
          .get(&response_id)
          .ok_or(VmError::TypeError("Response: invalid backing response"))?;
        Ok(res.headers.limits().max_response_body_bytes)
      })?;
      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::Bytes,
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if response_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, resp_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let result: std::result::Result<Vec<u8>, WebFetchError> =
    with_env_state_mut(env_id, scope.heap(), |state| {
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
      let byte_len = bytes.len();
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      let view = scope.alloc_uint8_array(ab, 0, byte_len)?;
      scope
        .heap_mut()
        .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
      vm.call_with_host_and_hooks(
        &mut *host,
        scope,
        host_hooks,
        cap.resolve,
        Value::Undefined,
        &[Value::Object(view)],
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
  let Value::Object(resp_obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  let (env_id, response_id) = response_info_from_this(scope, Value::Object(resp_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    Ok(res.body.is_some())
  })?;

  if !has_core_body {
    let cached = get_data_prop(scope, resp_obj, RESPONSE_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Response body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }

      let (max_bytes, content_type_result) = with_env_state(env_id, scope.heap(), |state| {
        let res = state
          .responses
          .get(&response_id)
          .ok_or(VmError::TypeError("Response: invalid backing response"))?;
        Ok((
          res.headers.limits().max_response_body_bytes,
          res.headers.get("Content-Type"),
        ))
      })?;
      let content_type = match content_type_result {
        Ok(v) => v,
        Err(err) => {
          let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
          let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.reject,
            Value::Undefined,
            &[err_value],
          )?;
          return Ok(cap.promise);
        }
      };
      let blob_type = content_type
        .as_deref()
        .map(normalize_content_type_for_blob)
        .unwrap_or_default();

      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::Blob { blob_type },
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if response_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, resp_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let (bytes_result, content_type_result): (
    std::result::Result<Vec<u8>, WebFetchError>,
    std::result::Result<Option<String>, WebFetchError>,
  ) = with_env_state_mut(env_id, scope.heap(), |state| {
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
        let proto = window_blob::blob_prototype_for_realm(realm_id).ok_or(
          VmError::Unimplemented("Response.blob requires Blob to be installed"),
        )?;
        window_blob::create_blob_with_proto(
          vm,
          scope,
          callee,
          proto,
          window_blob::BlobData {
            bytes,
            r#type: blob_type,
          },
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
  let Value::Object(resp_obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  let (env_id, response_id) = response_info_from_this(scope, Value::Object(resp_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    Ok(res.body.is_some())
  })?;

  if !has_core_body {
    let cached = get_data_prop(scope, resp_obj, RESPONSE_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Response body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }

      let (max_bytes, content_type_result) = with_env_state(env_id, scope.heap(), |state| {
        let res = state
          .responses
          .get(&response_id)
          .ok_or(VmError::TypeError("Response: invalid backing response"))?;
        Ok((
          res.headers.limits().max_response_body_bytes,
          res.headers.get("Content-Type"),
        ))
      })?;
      let content_type = match content_type_result {
        Ok(v) => v,
        Err(err) => {
          let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
          let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
          vm.call_with_host_and_hooks(
            &mut *host,
            scope,
            host_hooks,
            cap.reject,
            Value::Undefined,
            &[err_value],
          )?;
          return Ok(cap.promise);
        }
      };

      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::FormData { content_type },
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if response_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, resp_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let (bytes_result, content_type_result): (
    std::result::Result<Vec<u8>, WebFetchError>,
    std::result::Result<Option<String>, WebFetchError>,
  ) = with_env_state_mut(env_id, scope.heap(), |state| {
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
      let file_last_modified_ms =
        if content_type
          .as_deref()
          .is_some_and(|ct| normalize_content_type_for_blob(ct) == "multipart/form-data")
        {
          time::date_now_ms(scope)?
        } else {
          0
        };
      let entries =
        parse_form_data_entries_from_body(content_type.as_deref(), &bytes, file_last_modified_ms);
      match entries {
        Ok(entries) => {
          let form_data_result =
            window_form_data::create_form_data_with_entries(vm, scope, callee, entries);
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
                VmError::TypeError(msg) => {
                  create_type_error(vm, scope, &mut *host, host_hooks, msg)?
                }
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

fn json_to_js(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  value: &serde_json::Value,
) -> Result<Value, VmError> {
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
  let Value::Object(resp_obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  let (env_id, response_id) = response_info_from_this(scope, Value::Object(resp_obj))?;

  // Stream-backed body.
  let has_core_body = with_env_state(env_id, scope.heap(), |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    Ok(res.body.is_some())
  })?;

  if !has_core_body {
    let cached = get_data_prop(scope, resp_obj, RESPONSE_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if readable_stream_is_locked(vm, scope, host, host_hooks, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value =
          create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      if body_stream_is_used(scope, stream_obj)? {
        let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;
        let err_value = create_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Response body is already used",
        )?;
        vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          cap.reject,
          Value::Undefined,
          &[err_value],
        )?;
        return Ok(cap.promise);
      }
      let max_bytes = with_env_state(env_id, scope.heap(), |state| {
        let res = state
          .responses
          .get(&response_id)
          .ok_or(VmError::TypeError("Response: invalid backing response"))?;
        Ok(res.headers.limits().max_response_body_bytes)
      })?;
      return start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        None,
        max_bytes,
        StreamConsumeKind::Json,
      );
    }
  }

  let cap = new_promise_capability_for_env(vm, scope, &mut *host, host_hooks, env_id)?;

  if response_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, resp_obj)? {
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, "Response body is locked")?;
    vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      cap.reject,
      Value::Undefined,
      &[err_value],
    )?;
    return Ok(cap.promise);
  }

  let parsed: std::result::Result<serde_json::Value, WebFetchError> =
    with_env_state_mut(env_id, scope.heap(), |state| {
      let res = state
        .responses
        .get_mut(&response_id)
        .ok_or(VmError::TypeError("Response: invalid backing response"))?;
      let parsed = match res.body.as_mut() {
        Some(body) => body.json(),
        None => {
          // Fetch: "consume body" on a null body yields an empty byte sequence.
          // JSON parsing then fails with a SyntaxError (rather than a TypeError).
          let mut body = Body::empty();
          body.json()
        }
      };
      Ok(parsed)
    })?;

  match parsed {
    Ok(value) => {
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
    Err(err) => match err {
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

  if response_wrapper_cached_body_stream_is_locked(vm, scope, &mut *host, host_hooks, obj)? {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Response body is locked",
    ));
  }

  let (cloned, has_core_body) = with_env_state(env_id, scope.heap(), |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    let has_core_body = res.body.is_some();
    if res.body.as_ref().map_or(false, |b| b.body_used()) {
      Ok((None, has_core_body))
    } else {
      Ok((Some(res.clone()), has_core_body))
    }
  })?;

  let Some(cloned) = cloned else {
    return Err(throw_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "Response body is already used",
    ));
  };

  let mut cloned_body_stream: Option<GcObject> = None;
  if !has_core_body {
    let cached = get_data_prop(scope, obj, RESPONSE_BODY_STREAM_KEY)?;
    if let Value::Object(stream_obj) = cached {
      if body_stream_is_used(scope, stream_obj)? {
        return Err(throw_type_error(
          vm,
          scope,
          &mut *host,
          host_hooks,
          "Response body is already used",
        ));
      }
      let (s1, s2) = readable_stream_tee(vm, scope, host, host_hooks, stream_obj)?;
      ensure_body_stream_wrappers_installed(vm, scope, host, host_hooks, env_id, s1)?;
      ensure_body_stream_wrappers_installed(vm, scope, host, host_hooks, env_id, s2)?;
      set_data_prop(
        scope,
        obj,
        RESPONSE_BODY_STREAM_KEY,
        Value::Object(s1),
        false,
      )?;
      cloned_body_stream = Some(s2);
    }
  }

  let new_response_id = with_env_state_mut(env_id, scope.heap(), |state| {
    let id = state.alloc_id();
    state.responses.insert(id, cloned);
    Ok(id)
  })?;

  let proto = scope
    .heap()
    .object_prototype(obj)?
    .ok_or(VmError::InvariantViolation(
      "Response.prototype missing on instance",
    ))?;
  let resp_obj = make_response_wrapper(scope, env_id, headers_proto, proto, new_response_id)?;

  if let Some(stream_obj) = cloned_body_stream {
    set_data_prop(
      scope,
      resp_obj,
      RESPONSE_BODY_STREAM_KEY,
      Value::Object(stream_obj),
      false,
    )?;
  }
  Ok(Value::Object(resp_obj))
}

fn response_body_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(resp_obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  let (env_id, response_id) = response_info_from_this(scope, Value::Object(resp_obj))?;

  // Cache per Response instance.
  let cached = get_data_prop(scope, resp_obj, RESPONSE_BODY_STREAM_KEY)?;
  if let Value::Object(stream_obj) = cached {
    return Ok(Value::Object(stream_obj));
  }

  let has_body = with_env_state(env_id, scope.heap(), |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    Ok(res.body.is_some())
  })?;

  if !has_body {
    return Ok(Value::Null);
  }

  // See `request_body_get_native` for why we may need to synthesize a realm-aware callee.
  let streams_callee = if vm.current_realm().is_some() {
    callee
  } else {
    let (realm_id, call_id) = with_env_state(env_id, scope.heap(), |state| {
      Ok((state.realm_id, state.promise_executor_call))
    })?;
    let name_s = scope.alloc_string("__fastrender_streams_realm")?;
    scope.push_root(Value::String(name_s))?;
    let token_fn = scope.alloc_native_function_with_slots(
      call_id,
      None,
      name_s,
      0,
      &[Value::Number(realm_id.to_raw() as f64)],
    )?;
    scope.push_root(Value::Object(token_fn))?;
    token_fn
  };

  let stream_obj = window_streams::create_readable_byte_stream_lazy(vm, scope, streams_callee, move || {
    with_env_state_mut_no_sweep(env_id, |state| {
      let res = state
        .responses
        .get_mut(&response_id)
        .ok_or(VmError::TypeError("Response: invalid backing response"))?;
      match res.body.as_mut() {
        None => Ok(Vec::new()),
        Some(body) => {
          if body.body_used() {
            Ok(Vec::new())
          } else {
            body
              .consume_bytes()
              .map_err(|_| VmError::TypeError("Response body is already used"))
          }
        }
      }
    })
  })?;
  scope.push_root(Value::Object(stream_obj))?;

  with_env_state_mut(env_id, scope.heap(), |state| {
    state
      .response_body_stream_wrappers
      .insert(response_id, WeakGcObject::from(stream_obj));
    Ok(())
  })?;

  set_data_prop(
    scope,
    resp_obj,
    RESPONSE_BODY_STREAM_KEY,
    Value::Object(stream_obj),
    false,
  )?;

  Ok(Value::Object(stream_obj))
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
  let Value::Object(resp_obj) = this else {
    return Err(VmError::TypeError("Response: illegal invocation"));
  };
  let (env_id, response_id) = response_info_from_this(scope, Value::Object(resp_obj))?;
  let core_used = with_env_state(env_id, scope.heap(), |state| {
    let res = state
      .responses
      .get(&response_id)
      .ok_or(VmError::TypeError("Response: invalid backing response"))?;
    Ok(res.body.as_ref().map_or(false, |b| b.body_used()))
  })?;
  if core_used {
    return Ok(Value::Bool(true));
  }

  let cached = get_data_prop(scope, resp_obj, RESPONSE_BODY_STREAM_KEY)?;
  let Value::Object(stream_obj) = cached else {
    return Ok(Value::Bool(false));
  };
  Ok(Value::Bool(body_stream_is_used(scope, stream_obj)?))
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
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: FETCH_HEADERS_HOST_TAG,
      b: 0,
    },
  )?;
  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    HEADERS_KIND_KEY,
    Value::Number(kind as f64),
    false,
  )?;
  set_data_prop(
    scope,
    obj,
    HEADERS_OWNER_KEY,
    Value::Number(owner as f64),
    false,
  )?;
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
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: FETCH_REQUEST_HOST_TAG,
      b: 0,
    },
  )?;

  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    REQUEST_ID_KEY,
    Value::Number(request_id as f64),
    false,
  )?;

  let (method, url, mode, credentials, redirect, referrer, referrer_policy) =
    with_env_state(env_id, scope.heap(), |state| {
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
  set_data_prop(
    scope,
    obj,
    "credentials",
    Value::String(credentials_s),
    false,
  )?;
  let redirect_s = scope.alloc_string(request_redirect_to_string(redirect))?;
  set_data_prop(scope, obj, "redirect", Value::String(redirect_s), false)?;

  // https://fetch.spec.whatwg.org/#dom-request-referrer
  //
  // FastRender stores referrer state as:
  // - "" => "client" referrer state
  // - "no-referrer" => explicit omission
  // - otherwise => absolute URL
  let js_referrer = if referrer == "no-referrer" {
    ""
  } else if referrer.is_empty() {
    "about:client"
  } else {
    &referrer
  };
  let referrer_s = scope.alloc_string(js_referrer)?;
  set_data_prop(scope, obj, "referrer", Value::String(referrer_s), false)?;
  let referrer_policy_s = scope.alloc_string(referrer_policy.as_str())?;
  set_data_prop(
    scope,
    obj,
    "referrerPolicy",
    Value::String(referrer_policy_s),
    false,
  )?;

  // Fetch-only Request attributes that are not currently represented in `CoreRequest`.
  //
  // These are WebIDL `readonly` attributes in the platform API; we expose them as per-instance
  // non-writable data properties for now.
  let destination_s = scope.alloc_string("")?;
  set_data_prop(scope, obj, "destination", Value::String(destination_s), false)?;
  let cache_s = scope.alloc_string("default")?;
  set_data_prop(scope, obj, "cache", Value::String(cache_s), false)?;
  let integrity_s = scope.alloc_string("")?;
  set_data_prop(scope, obj, "integrity", Value::String(integrity_s), false)?;
  set_data_prop(scope, obj, "keepalive", Value::Bool(false), false)?;
  set_data_prop(scope, obj, "isReloadNavigation", Value::Bool(false), false)?;
  set_data_prop(scope, obj, "isHistoryNavigation", Value::Bool(false), false)?;

  let headers_obj = make_headers_wrapper(
    scope,
    env_id,
    headers_proto,
    HEADERS_KIND_REQUEST,
    request_id,
  )?;
  set_data_prop(scope, obj, "headers", Value::Object(headers_obj), false)?;

  with_env_state_mut(env_id, scope.heap(), |state| {
    state.request_wrappers.insert(
      request_id,
      RequestWrapperState {
        request: WeakGcObject::from(obj),
        headers: WeakGcObject::from(headers_obj),
      },
    );
    Ok(())
  })?;

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
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: FETCH_RESPONSE_HOST_TAG,
      b: 0,
    },
  )?;

  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(
    scope,
    obj,
    RESPONSE_ID_KEY,
    Value::Number(response_id as f64),
    false,
  )?;

  let (status, url, status_text, r#type, redirected) = with_env_state(env_id, scope.heap(), |state| {
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
  // Fetch's `Response.url` getter serializes the response URL with `exclude fragment = true`.
  // We store it as a data property for simplicity, so strip the fragment at wrapper creation.
  let url = if url.is_empty() {
    url
  } else if let Ok(mut parsed) = ::url::Url::parse(&url) {
    parsed.set_fragment(None);
    parsed.to_string()
  } else {
    url.split('#').next().unwrap_or("").to_string()
  };
  let url_s = scope.alloc_string(&url)?;
  let st_s = scope.alloc_string(&status_text)?;
  set_data_prop(scope, obj, "url", Value::String(url_s), false)?;
  set_data_prop(scope, obj, "statusText", Value::String(st_s), false)?;
  let type_s = scope.alloc_string(response_type_to_string(r#type))?;
  set_data_prop(scope, obj, "type", Value::String(type_s), false)?;
  set_data_prop(scope, obj, "redirected", Value::Bool(redirected), false)?;

  let headers_obj = make_headers_wrapper(
    scope,
    env_id,
    headers_proto,
    HEADERS_KIND_RESPONSE,
    response_id,
  )?;
  set_data_prop(scope, obj, "headers", Value::Object(headers_obj), false)?;

  with_env_state_mut(env_id, scope.heap(), |state| {
    state.response_wrappers.insert(
      response_id,
      ResponseWrapperState {
        response: WeakGcObject::from(obj),
        headers: WeakGcObject::from(headers_obj),
      },
    );
    Ok(())
  })?;

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
  let limits = with_env_state(env_id, scope.heap(), |state| Ok(state.env.limits.clone()))?;

  let init = args.get(1).copied().unwrap_or(Value::Undefined);
  let mut status: u16 = 200;
  let mut status_text = String::new();
  let mut headers = CoreHeaders::new_with_guard_and_limits(HeadersGuard::Response, &limits);

  let body = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut body_bytes: Option<Vec<u8>> = None;
  let mut body_stream: Option<GcObject> = None;
  let mut inferred_content_type: Option<String> = None;
  if !matches!(body, Value::Undefined | Value::Null) {
    let max_body_bytes = headers.limits().max_response_body_bytes;
    match body {
      Value::Object(obj) => {
        if is_readable_stream_like_object(vm, scope, host, host_hooks, obj)? {
          body_stream = Some(obj);
          ensure_body_stream_wrappers_installed(vm, scope, host, host_hooks, env_id, obj)?;
        } else {
          let bytes: Vec<u8> = if scope.heap().is_array_buffer_object(obj) {
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
          } else if scope.heap().is_typed_array_object(obj) {
            // BufferSource (ArrayBufferView): copy the visible bytes.
            let byte_length = scope.heap().typed_array_byte_length(obj)?;
            if byte_length > max_body_bytes {
              return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
            }
            let byte_offset = scope.heap().typed_array_byte_offset(obj)?;
            let buffer = scope.heap().typed_array_buffer(obj)?;
            let data = scope.heap().array_buffer_data(buffer)?;
            let end = byte_offset
              .checked_add(byte_length)
              .ok_or(VmError::TypeError("TypedArray byte offset overflow"))?;
            let slice = data
              .get(byte_offset..end)
              .ok_or(VmError::TypeError("TypedArray view out of bounds"))?;
            slice.to_vec()
          } else if scope.heap().is_data_view_object(obj) {
            // BufferSource (DataView): copy the visible bytes.
            let byte_length = scope.heap().data_view_byte_length(obj)?;
            if byte_length > max_body_bytes {
              return Err(VmError::TypeError(FETCH_BODY_TOO_LONG_ERROR));
            }
            let byte_offset = scope.heap().data_view_byte_offset(obj)?;
            let buffer = scope.heap().data_view_buffer(obj)?;
            let data = scope.heap().array_buffer_data(buffer)?;
            let end = byte_offset
              .checked_add(byte_length)
              .ok_or(VmError::TypeError("DataView byte offset overflow"))?;
            let slice = data
              .get(byte_offset..end)
              .ok_or(VmError::TypeError("DataView view out of bounds"))?;
            slice.to_vec()
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
            window_blob::clone_blob_data_for_fetch(vm, scope.heap(), body)?
          {
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
            let boundary_id = with_env_state_mut(env_id, scope.heap(), |state| {
              let id = state.multipart_boundary_counter;
              state.multipart_boundary_counter = state.multipart_boundary_counter.saturating_add(1);
              Ok(id)
            })?;
            let boundary = format!("----fastrenderformdata{boundary_id}");
            let multipart = encode_form_data_as_multipart(
              &entries,
              &boundary,
              max_body_bytes,
              FETCH_BODY_TOO_LONG_ERROR,
            )?;
            inferred_content_type = Some(format!("multipart/form-data; boundary={boundary}"));
            multipart
          } else {
            inferred_content_type = Some("text/plain;charset=UTF-8".to_string());
            let s = scope.to_string(vm, host, host_hooks, body)?;
            js_string_to_rust_string_limited(
              scope.heap(),
              s,
              max_body_bytes,
              FETCH_BODY_TOO_LONG_ERROR,
            )?
            .into_bytes()
          };
          body_bytes = Some(bytes);
        }
      }
      other => {
        inferred_content_type = Some("text/plain;charset=UTF-8".to_string());
        let s = scope.to_string(vm, host, host_hooks, other)?;
        body_bytes = Some(
          js_string_to_rust_string_limited(
            scope.heap(),
            s,
            max_body_bytes,
            FETCH_BODY_TOO_LONG_ERROR,
          )?
          .into_bytes(),
        );
      }
    }
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
      fill_headers_from_init(
        vm,
        scope,
        &mut *host,
        host_hooks,
        env_id,
        &mut headers,
        headers_val,
      )?;
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
  if (body_bytes.is_some() || body_stream.is_some()) && matches!(status, 101 | 103 | 204 | 205 | 304) {
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

  let response_id = with_env_state_mut(env_id, scope.heap(), |state| {
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

  if let Some(stream_obj) = body_stream {
    set_data_prop(
      scope,
      obj,
      RESPONSE_BODY_STREAM_KEY,
      Value::Object(stream_obj),
      false,
    )?;
  }
  Ok(Value::Object(obj))
}

fn remove_fetch_roots(heap: &mut Heap, resolve_root: RootId, reject_root: RootId, promise_root: RootId, signal_root: Option<RootId>) {
  heap.remove_root(resolve_root);
  heap.remove_root(reject_root);
  heap.remove_root(promise_root);
  if let Some(signal_root) = signal_root {
    heap.remove_root(signal_root);
  }
}

fn enqueue_fetch_network_task<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  env_id: u64,
  request: CoreRequest,
  object_url_origin: String,
  headers_proto: GcObject,
  response_proto: GcObject,
  resolve_root: RootId,
  reject_root: RootId,
  promise_root: RootId,
  signal_root: Option<RootId>,
) -> Result<(), VmError> {
  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(host_hooks) else {
    let reject_val = scope
      .heap()
      .get_root(reject_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    let err = create_type_error(
      vm,
      scope,
      &mut *host,
      host_hooks,
      "fetch called without an active EventLoop",
    )?;
    let call_result =
      vm.call_with_host_and_hooks(&mut *host, scope, host_hooks, reject_val, Value::Undefined, &[err]);
    remove_fetch_roots(scope.heap_mut(), resolve_root, reject_root, promise_root, signal_root);
    call_result?;
    return Ok(());
  };

  let enqueue_result = event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
    // Execute `execute_web_fetch` synchronously on this networking task.
    let (fetcher, document_url, document_origin, referrer_policy) = match with_env_state(
      env_id,
      host.window_realm()?.heap(),
      |state| {
        let env = &state.env;
        Ok((
          Arc::clone(&env.fetcher),
          env.document_url.clone(),
          env.document_origin.clone(),
          env.referrer_policy,
        ))
      },
    ) {
      Ok(tuple) => tuple,
      Err(err) => {
        let message = format!("fetch failed: {err}");
        let queue_result = event_loop.queue_microtask(move |host, event_loop| {
          let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
          hooks.set_event_loop(event_loop);
          let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
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
            let type_error = create_type_error(&mut vm, &mut scope, vm_host, &mut hooks, &message)?;
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
          let window_realm = host.window_realm()?;
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
      let window_realm = host.window_realm()?;
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
          let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
          hooks.set_event_loop(event_loop);
          let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
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
          let window_realm = host.window_realm()?;
          window_realm.heap_mut().remove_root(resolve_root);
          window_realm.heap_mut().remove_root(reject_root);
          window_realm.heap_mut().remove_root(promise_root);
          window_realm.heap_mut().remove_root(signal_root_id);
          return Err(queue_err);
        }

        return Ok(());
      }
    }

    let result = if request.url.starts_with("blob:") {
      execute_blob_url_fetch(&request, &object_url_origin)
    } else {
      let exec_ctx = WebFetchExecutionContext {
        destination: FetchDestination::Fetch,
        referrer_url: document_url.as_deref(),
        client_origin: document_origin.as_ref(),
        referrer_policy,
        csp: None,
      };

      execute_web_fetch(fetcher.as_ref(), &request, exec_ctx)
    };

    match result {
      Ok(mut response) => {
        // JS `Response.headers` for fetch() results is immutable in browsers.
        response.headers.set_guard(HeadersGuard::Immutable);

        // If the signal was aborted while the underlying fetch was running, reject instead of
        // storing/settling with a `Response`.
        //
        // Note: this cannot stop the already-running synchronous `ResourceFetcher` call; we can
        // only change the JS-visible outcome.
        if let Some(signal_root_id) = signal_root {
          let window_realm = host.window_realm()?;
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
              let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
              hooks.set_event_loop(event_loop);
              let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
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
              let window_realm = host.window_realm()?;
              window_realm.heap_mut().remove_root(resolve_root);
              window_realm.heap_mut().remove_root(reject_root);
              window_realm.heap_mut().remove_root(promise_root);
              window_realm.heap_mut().remove_root(signal_root_id);
              return Err(queue_err);
            }

            return Ok(());
          }
        }

        let response_id = match with_env_state_mut(env_id, host.window_realm()?.heap(), |state| {
          let id = state.alloc_id();
          state.responses.insert(id, response);
          Ok(id)
        }) {
          Ok(id) => id,
          Err(err) => {
            let message = format!("fetch failed: {err}");
            let queue_result = event_loop.queue_microtask(move |host, event_loop| {
              let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
              hooks.set_event_loop(event_loop);
              let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
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
                let type_error = create_type_error(&mut vm, &mut scope, vm_host, &mut hooks, &message)?;
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
              let window_realm = host.window_realm()?;
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
          let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
          let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
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
            let reject = heap
              .get_root(reject_root)
              .ok_or_else(|| VmError::invalid_handle())?;
            // `Scope` holds a mutable borrow of the heap, so extract any rooted values we need
            // beforehand.
            let signal_obj = signal_root
              .and_then(|signal_root_id| heap.get_root(signal_root_id))
              .and_then(|signal_value| match signal_value {
                Value::Object(obj) => Some(obj),
                _ => None,
              });

            // If the signal was aborted after the networking task ran but before this completion
            // microtask settles the promise, reject instead of resolving.
            if let Some(signal_obj) = signal_obj {
              let aborted = {
                let mut scope = heap.scope();
                let aborted_key = alloc_key(&mut scope, "aborted")?;
                let aborted_val =
                  vm.get_with_host_and_hooks(vm_host, &mut scope, &mut hooks, signal_obj, aborted_key)?;
                scope.heap().to_boolean(aborted_val)?
              };

              if aborted {
                // Ensure we don't leak the stored response backing state.
                let _ = with_env_state_mut(env_id, heap, |state| {
                  state.responses.remove(&response_id);
                  Ok(())
                });

                let mut scope = heap.scope();
                let reason_key = alloc_key(&mut scope, "reason")?;
                let reason =
                  vm.get_with_host_and_hooks(vm_host, &mut scope, &mut hooks, signal_obj, reason_key)?;
                vm.call_with_host_and_hooks(
                  vm_host,
                  &mut scope,
                  &mut hooks,
                  reject,
                  Value::Undefined,
                  &[reason],
                )?;
                return Ok(());
              }
            }

            let mut scope = heap.scope();

            let resp_obj = make_response_wrapper(
              &mut scope,
              env_id,
              headers_proto,
              response_proto,
              response_id,
            )?;

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

          // If wrapper construction failed, clean up the backing store entry. In this code path
          // the `CoreResponse` is inserted into env state before the JS wrapper exists, so there is
          // no `response_wrappers` entry yet; without this cleanup we'd retain the response forever.
          if call_result.is_err() {
            let _ = with_env_state_mut(env_id, heap, |state| {
              if !state.response_wrappers.contains_key(&response_id) {
                state.responses.remove(&response_id);
                state.response_body_stream_wrappers.remove(&response_id);
              }
              Ok(())
            });
          }

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
          let _ = with_env_state_mut(env_id, host.window_realm()?.heap(), |state| {
            state.responses.remove(&response_id);
            Ok(())
          });
          let window_realm = host.window_realm()?;
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
          let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
          hooks.set_event_loop(event_loop);
          let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
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
            let type_error = create_type_error(&mut vm, &mut scope, vm_host, &mut hooks, &message)?;
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
          let window_realm = host.window_realm()?;
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
    let reject_val = scope
      .heap()
      .get_root(reject_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    let err_value = create_type_error(vm, scope, &mut *host, host_hooks, &err.to_string())?;
    let call_result = vm.call_with_host_and_hooks(
      &mut *host,
      scope,
      host_hooks,
      reject_val,
      Value::Undefined,
      &[err_value],
    );
    remove_fetch_roots(scope.heap_mut(), resolve_root, reject_root, promise_root, signal_root);
    call_result?;
  }

  Ok(())
}

fn fetch_body_stream_then_info_from_callee(scope: &Scope<'_>, callee: GcObject) -> Result<(u64, u64), VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let env_id = slots
    .get(0)
    .copied()
    .ok_or(VmError::InvariantViolation(
      "fetch body stream callback missing env id slot",
    ))?;
  let pending_id = slots
    .get(1)
    .copied()
    .ok_or(VmError::InvariantViolation(
      "fetch body stream callback missing state id slot",
    ))?;
  let env_id = number_to_u64(env_id).map_err(|_| VmError::InvariantViolation("invalid env id slot"))?;
  let pending_id =
    number_to_u64(pending_id).map_err(|_| VmError::InvariantViolation("invalid state id slot"))?;
  Ok((env_id, pending_id))
}

fn fetch_body_stream_then_fulfilled_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, pending_id) = fetch_body_stream_then_info_from_callee(scope, callee)?;
  let Some(mut pending) = with_env_state_mut(env_id, scope.heap(), |state| {
    Ok(state.pending_fetch_stream_bodies.remove(&pending_id))
  })?
  else {
    return Ok(Value::Undefined);
  };

  // If the signal was aborted during stream buffering, reject instead of dispatching the request.
  if let Some(signal_root) = pending.signal_root {
    if let Some(Value::Object(signal_obj)) = scope.heap().get_root(signal_root) {
      scope.push_root(Value::Object(signal_obj))?;
      let aborted_key = alloc_key(scope, "aborted")?;
      let aborted = vm.get_with_host_and_hooks(host, scope, host_hooks, signal_obj, aborted_key)?;
      if scope.heap().to_boolean(aborted)? {
        let reason_key = alloc_key(scope, "reason")?;
        let reason = vm.get_with_host_and_hooks(host, scope, host_hooks, signal_obj, reason_key)?;
        let reject_val = scope
          .heap()
          .get_root(pending.reject_root)
          .ok_or_else(|| VmError::invalid_handle())?;
        let call_result = vm.call_with_host_and_hooks(
          host,
          scope,
          host_hooks,
          reject_val,
          Value::Undefined,
          &[reason],
        );
        remove_fetch_roots(
          scope.heap_mut(),
          pending.resolve_root,
          pending.reject_root,
          pending.promise_root,
          pending.signal_root,
        );
        call_result?;
        return Ok(Value::Undefined);
      }
    }
  }

  let bytes_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let bytes: Vec<u8> = match bytes_val {
    Value::Object(obj) if scope.heap().is_array_buffer_object(obj) => {
      scope.heap().array_buffer_data(obj)?.to_vec()
    }
    Value::Object(obj) if scope.heap().is_uint8_array_object(obj) => scope.heap().uint8_array_data(obj)?.to_vec(),
    _ => {
      let reject_val = scope
        .heap()
        .get_root(pending.reject_root)
        .ok_or_else(|| VmError::invalid_handle())?;
      let err = create_type_error(
        vm,
        scope,
        host,
        host_hooks,
        "ReadableStream request body must resolve to an ArrayBuffer",
      )?;
      let call_result = vm.call_with_host_and_hooks(
        host,
        scope,
        host_hooks,
        reject_val,
        Value::Undefined,
        &[err],
      );
      remove_fetch_roots(
        scope.heap_mut(),
        pending.resolve_root,
        pending.reject_root,
        pending.promise_root,
        pending.signal_root,
      );
      call_result?;
      return Ok(Value::Undefined);
    }
  };

  // Defensive limit enforcement (stream consumer should already enforce this).
  if bytes.len() > pending.request.headers.limits().max_request_body_bytes {
    let reject_val = scope
      .heap()
      .get_root(pending.reject_root)
      .ok_or_else(|| VmError::invalid_handle())?;
    let err = create_type_error(vm, scope, host, host_hooks, FETCH_BODY_TOO_LONG_ERROR)?;
    let call_result = vm.call_with_host_and_hooks(
      host,
      scope,
      host_hooks,
      reject_val,
      Value::Undefined,
      &[err],
    );
    remove_fetch_roots(
      scope.heap_mut(),
      pending.resolve_root,
      pending.reject_root,
      pending.promise_root,
      pending.signal_root,
    );
    call_result?;
    return Ok(Value::Undefined);
  }

  let body = match Body::new_with_limits(bytes, pending.request.headers.limits()) {
    Ok(b) => b,
    Err(err) => {
      let reject_val = scope
        .heap()
        .get_root(pending.reject_root)
        .ok_or_else(|| VmError::invalid_handle())?;
      let err_value = create_type_error(vm, scope, host, host_hooks, &err.to_string())?;
      let call_result = vm.call_with_host_and_hooks(
        host,
        scope,
        host_hooks,
        reject_val,
        Value::Undefined,
        &[err_value],
      );
      remove_fetch_roots(
        scope.heap_mut(),
        pending.resolve_root,
        pending.reject_root,
        pending.promise_root,
        pending.signal_root,
      );
      call_result?;
      return Ok(Value::Undefined);
    }
  };

  pending.request.body = Some(body);

  enqueue_fetch_network_task::<Host>(
    vm,
    scope,
    host,
    host_hooks,
    env_id,
    pending.request,
    pending.object_url_origin,
    pending.headers_proto,
    pending.response_proto,
    pending.resolve_root,
    pending.reject_root,
    pending.promise_root,
    pending.signal_root,
  )?;
  Ok(Value::Undefined)
}

fn fetch_body_stream_then_rejected_native<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, pending_id) = fetch_body_stream_then_info_from_callee(scope, callee)?;
  let Some(pending) = with_env_state_mut(env_id, scope.heap(), |state| {
    Ok(state.pending_fetch_stream_bodies.remove(&pending_id))
  })?
  else {
    return Ok(Value::Undefined);
  };
  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  let reject_val = scope
    .heap()
    .get_root(pending.reject_root)
    .ok_or_else(|| VmError::invalid_handle())?;
  let call_result = vm.call_with_host_and_hooks(
    host,
    scope,
    host_hooks,
    reject_val,
    Value::Undefined,
    &[reason],
  );
  remove_fetch_roots(
    scope.heap_mut(),
    pending.resolve_root,
    pending.reject_root,
    pending.promise_root,
    pending.signal_root,
  );
  call_result?;
  Ok(Value::Undefined)
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
  let limits = with_env_state(env_id, scope.heap(), |state| Ok(state.env.limits.clone()))?;
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let init = args.get(1).copied().unwrap_or(Value::Undefined);

  // Build request synchronously (invalid init should reject deterministically).
  let input_request_info = request_info_from_value(scope, input);
  let input_request_obj = match (input_request_info, input) {
    (Some(_), Value::Object(obj)) => Some(obj),
    _ => None,
  };

  let mut request = if let Some((other_env_id, other_request_id)) = input_request_info {
    let cloned: Option<CoreRequest> = with_env_state(other_env_id, scope.heap(), |state| {
      let req = state
        .requests
        .get(&other_request_id)
        .ok_or(VmError::TypeError("Request: invalid backing request"))?;
      if req.body.as_ref().map_or(false, |b| b.body_used()) {
        Ok(None)
      } else {
        Ok(Some(req.clone()))
      }
    })?;
    match cloned {
      Some(req) => req,
      None => {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "Request body is already used",
        ));
      }
    }
  } else {
    let url = to_rust_string_limited(
      scope.heap_mut(),
      input,
      limits.max_url_bytes,
      FETCH_URL_TOO_LONG_ERROR,
    )?;
    let base_url = current_document_base_url(vm, scope.heap(), env_id)?;
    let url = resolve_url(&url, base_url.as_deref())
      .map_err(|err| throw_type_error(vm, scope, host, host_hooks, &err.to_string()))?;
    CoreRequest::new_with_limits("GET", url, &limits)
  };

  // Wrapper-only Request attributes that are not stored in the core request type.
  let mut cache = "default".to_string();
  let mut integrity = String::new();
  let mut keepalive = false;
  if let Some(input_obj) = input_request_obj {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(input_obj))?;

    let cache_val = get_data_prop(&mut scope, input_obj, "cache")?;
    if !matches!(cache_val, Value::Undefined) {
      cache = to_rust_string_limited(scope.heap_mut(), cache_val, 64, FETCH_CACHE_TOO_LONG_ERROR)?;
    }

    let integrity_val = get_data_prop(&mut scope, input_obj, "integrity")?;
    if !matches!(integrity_val, Value::Undefined) {
      integrity = to_rust_string_limited(
        scope.heap_mut(),
        integrity_val,
        limits.max_url_bytes,
        FETCH_INTEGRITY_TOO_LONG_ERROR,
      )?;
    }

    let keepalive_val = get_data_prop(&mut scope, input_obj, "keepalive")?;
    if !matches!(keepalive_val, Value::Undefined) {
      keepalive = scope.heap().to_boolean(keepalive_val)?;
    }
  }

  let mut body_stream: Option<GcObject> = None;
  let init_body_provided = apply_request_init(
    vm,
    scope,
    host,
    host_hooks,
    env_id,
    &limits,
    &mut request,
    &mut body_stream,
    &mut cache,
    &mut integrity,
    &mut keepalive,
    init,
  )?;

  // Request constructor invariant (fetch shares the same construction algorithm): the resulting
  // mode must not be "navigate", even when inherited from an input Request.
  if request.mode == RequestMode::Navigate {
    return Err(throw_type_error(
      vm,
      scope,
      host,
      host_hooks,
      "Request.mode \"navigate\" is not allowed",
    ));
  }

  // Enforce invariants even when `init` is omitted (e.g. `fetch(existingRequest)`).
  request.method =
    normalize_and_validate_method(vm, scope, host, host_hooks, request.method.as_str())?;
  if request.method.eq_ignore_ascii_case("GET") || request.method.eq_ignore_ascii_case("HEAD") {
    if request.body.is_some() || body_stream.is_some() {
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

  if !init_body_provided {
    if let Some(input_obj) = input_request_obj {
      if request_wrapper_cached_body_stream_is_locked(vm, scope, host, host_hooks, input_obj)? {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          host_hooks,
          "Request body is locked",
        ));
      }

      if request.body.is_none() && body_stream.is_none() {
        let cached = get_data_prop(scope, input_obj, REQUEST_BODY_STREAM_KEY)?;
        if let Value::Object(input_stream) = cached {
          if body_stream_is_used(scope, input_stream)? {
            return Err(throw_type_error(
              vm,
              scope,
              host,
              host_hooks,
              "Request body is already used",
            ));
          }
          body_stream = Some(input_stream);
        }
      }
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
      return Err(VmError::InvariantViolation(
        "Request init must be an object",
      ));
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

  // `blob:` object URLs are origin-scoped. Capture the current origin so the networking task can
  // enforce same-origin access without needing to touch the JS heap.
  let object_url_origin = current_document_origin_for_object_urls(vm, scope.heap(), env_id)?;

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

  // If the request body is an external ReadableStream, buffer it into bounded in-memory bytes
  // before dispatching the underlying Rust fetch implementation.
  if let Some(stream_obj) = body_stream {
    // Stream buffering relies on Promise jobs, which are only executed when an EventLoop is active.
    // If no EventLoop is present, reject like the non-stream path.
    if event_loop_mut_from_hooks::<Host>(host_hooks).is_none() {
      enqueue_fetch_network_task::<Host>(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        request,
        object_url_origin,
        headers_proto,
        response_proto,
        resolve_root,
        reject_root,
        promise_root,
        signal_root,
      )?;
      return Ok(promise);
    }

    let max_bytes = request.headers.limits().max_request_body_bytes;
    let abort_signal_obj = signal_root
      .and_then(|root_id| scope.heap().get_root(root_id))
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      });

    let (pending_id, on_fulfilled_call, on_rejected_call) = with_env_state_mut(env_id, scope.heap(), |state| {
      let id = state.alloc_id();
      let on_fulfilled_call = state.fetch_body_stream_then_fulfilled_call;
      let on_rejected_call = state.fetch_body_stream_then_rejected_call;
      state.pending_fetch_stream_bodies.insert(
        id,
        PendingFetchStreamBodyState {
          request,
          object_url_origin,
          headers_proto,
          response_proto,
          resolve_root,
          reject_root,
          promise_root,
          signal_root,
        },
      );
      Ok((id, on_fulfilled_call, on_rejected_call))
    })?;

    let setup_result: Result<(), VmError> = (|| {
      // Build then callbacks.
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      let func_proto = intr.function_prototype();

      let on_fulfilled_name = scope.alloc_string("onFetchBodyBuffered")?;
      scope.push_root(Value::String(on_fulfilled_name))?;
      let on_fulfilled = scope.alloc_native_function_with_slots(
        on_fulfilled_call,
        None,
        on_fulfilled_name,
        1,
        &[Value::Number(env_id as f64), Value::Number(pending_id as f64)],
      )?;
      scope
        .heap_mut()
        .object_set_prototype(on_fulfilled, Some(func_proto))?;

      let on_rejected_name = scope.alloc_string("onFetchBodyRejected")?;
      scope.push_root(Value::String(on_rejected_name))?;
      let on_rejected = scope.alloc_native_function_with_slots(
        on_rejected_call,
        None,
        on_rejected_name,
        1,
        &[Value::Number(env_id as f64), Value::Number(pending_id as f64)],
      )?;
      scope
        .heap_mut()
        .object_set_prototype(on_rejected, Some(func_proto))?;

      // Start consuming into an ArrayBuffer.
      let consume_promise = start_consuming_body_stream(
        vm,
        scope,
        host,
        host_hooks,
        env_id,
        stream_obj,
        abort_signal_obj,
        max_bytes,
        StreamConsumeKind::ArrayBuffer,
      )?;
      let Value::Object(promise_obj) = consume_promise else {
        return Err(VmError::TypeError(
          "ReadableStream buffering must return a Promise object",
        ));
      };

      // consumePromise.then(on_fulfilled, on_rejected)
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(promise_obj))?;
      scope.push_root(Value::Object(on_fulfilled))?;
      scope.push_root(Value::Object(on_rejected))?;
      let then_key = alloc_key(&mut scope, "then")?;
      let then_fn = vm.get_with_host_and_hooks(host, &mut scope, host_hooks, promise_obj, then_key)?;
      vm.call_with_host_and_hooks(
        host,
        &mut scope,
        host_hooks,
        then_fn,
        Value::Object(promise_obj),
        &[Value::Object(on_fulfilled), Value::Object(on_rejected)],
      )?;

      Ok(())
    })();

    if let Err(err) = setup_result {
      // Best-effort cleanup: remove pending state and reject the returned fetch Promise.
      if let Some(pending) = with_env_state_mut(env_id, scope.heap(), |state| {
        Ok(state.pending_fetch_stream_bodies.remove(&pending_id))
      })? {
        let reject_val = scope
          .heap()
          .get_root(pending.reject_root)
          .ok_or_else(|| VmError::invalid_handle())?;
        let err_value = match err {
          VmError::Throw(thrown) => thrown,
          other => create_type_error(vm, scope, &mut *host, host_hooks, &other.to_string())?,
        };
        let call_result = vm.call_with_host_and_hooks(
          &mut *host,
          scope,
          host_hooks,
          reject_val,
          Value::Undefined,
          &[err_value],
        );
        remove_fetch_roots(
          scope.heap_mut(),
          pending.resolve_root,
          pending.reject_root,
          pending.promise_root,
          pending.signal_root,
        );
        call_result?;
      }
    }

    return Ok(promise);
  }

  enqueue_fetch_network_task::<Host>(
    vm,
    scope,
    host,
    host_hooks,
    env_id,
    request,
    object_url_origin,
    headers_proto,
    response_proto,
    resolve_root,
    reject_root,
    promise_root,
    signal_root,
  )?;

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
  let global = realm.global_object();
  let promise_executor_call = vm.register_native_call(promise_capability_executor_call)?;
  let body_stream_get_reader_wrapper_call = vm.register_native_call(body_stream_get_reader_wrapper_native)?;
  let body_stream_cancel_wrapper_call = vm.register_native_call(body_stream_cancel_wrapper_native)?;
  let body_stream_reader_read_wrapper_call = vm.register_native_call(body_stream_reader_read_wrapper_native)?;
  let body_stream_reader_cancel_wrapper_call = vm.register_native_call(body_stream_reader_cancel_wrapper_native)?;
  let stream_consume_fulfilled_call = vm.register_native_call(stream_consume_fulfilled_native)?;
  let stream_consume_rejected_call = vm.register_native_call(stream_consume_rejected_native)?;
  let fetch_body_stream_then_fulfilled_call =
    vm.register_native_call(fetch_body_stream_then_fulfilled_native::<Host>)?;
  let fetch_body_stream_then_rejected_call =
    vm.register_native_call(fetch_body_stream_then_rejected_native::<Host>)?;
  {
    let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.insert(
      env_id,
      EnvState::new(
        env,
        realm.id(),
        WeakGcObject::from(global),
        promise_executor_call,
        body_stream_get_reader_wrapper_call,
        body_stream_cancel_wrapper_call,
        body_stream_reader_read_wrapper_call,
        body_stream_reader_cancel_wrapper_call,
        stream_consume_fulfilled_call,
        stream_consume_rejected_call,
        fetch_body_stream_then_fulfilled_call,
        fetch_body_stream_then_rejected_call,
        heap.gc_runs(),
      ),
    );
  }

  let bindings = WindowFetchBindings::new(env_id);

  let mut scope = heap.scope();
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
    scope
      .heap_mut()
      .object_set_prototype(ctor, Some(func_proto))?;
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
    scope
      .heap_mut()
      .object_set_prototype(append, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "append", Value::Object(append), true)?;

    let set_id = vm.register_native_call(headers_set_native)?;
    let set_name = scope.alloc_string("set")?;
    scope.push_root(Value::String(set_name))?;
    let set_fn = scope.alloc_native_function(set_id, None, set_name, 2)?;
    scope
      .heap_mut()
      .object_set_prototype(set_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "set", Value::Object(set_fn), true)?;

    let get_id = vm.register_native_call(headers_get_native)?;
    let get_name = scope.alloc_string("get")?;
    scope.push_root(Value::String(get_name))?;
    let get_fn = scope.alloc_native_function(get_id, None, get_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(get_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "get", Value::Object(get_fn), true)?;

    let get_set_cookie_id = vm.register_native_call(headers_get_set_cookie_native)?;
    let get_set_cookie_name = scope.alloc_string("getSetCookie")?;
    scope.push_root(Value::String(get_set_cookie_name))?;
    let get_set_cookie_fn =
      scope.alloc_native_function(get_set_cookie_id, None, get_set_cookie_name, 0)?;
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
    scope
      .heap_mut()
      .object_set_prototype(has_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "has", Value::Object(has_fn), true)?;

    let delete_id = vm.register_native_call(headers_delete_native)?;
    let delete_name = scope.alloc_string("delete")?;
    scope.push_root(Value::String(delete_name))?;
    let delete_fn = scope.alloc_native_function(delete_id, None, delete_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(delete_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "delete", Value::Object(delete_fn), true)?;

    let for_each_id = vm.register_native_call(headers_for_each_native)?;
    let for_each_name = scope.alloc_string("forEach")?;
    scope.push_root(Value::String(for_each_name))?;
    let for_each_fn = scope.alloc_native_function(for_each_id, None, for_each_name, 1)?;
    scope
      .heap_mut()
      .object_set_prototype(for_each_fn, Some(func_proto))?;
    set_data_prop(
      &mut scope,
      proto,
      "forEach",
      Value::Object(for_each_fn),
      true,
    )?;

    // Deterministic iteration for `Headers` (`entries`/`keys`/`values` + @@iterator).
    let iter_proto = {
      let object_proto = realm.intrinsics().object_prototype();
      let iter_proto = scope.alloc_object_with_prototype(Some(object_proto))?;
      scope.push_root(Value::Object(iter_proto))?;

      let next_id = vm.register_native_call(headers_iterator_next_native)?;
      let next_name = scope.alloc_string("next")?;
      scope.push_root(Value::String(next_name))?;
      let next_fn = scope.alloc_native_function(next_id, None, next_name, 0)?;
      scope
        .heap_mut()
        .object_set_prototype(next_fn, Some(func_proto))?;
      set_data_prop(&mut scope, iter_proto, "next", Value::Object(next_fn), true)?;

      let iter_id = vm.register_native_call(headers_iterator_iterator_native)?;
      let iter_name = scope.alloc_string("Symbol.iterator")?;
      scope.push_root(Value::String(iter_name))?;
      let iter_fn = scope.alloc_native_function(iter_id, None, iter_name, 0)?;
      scope.push_root(Value::Object(iter_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(iter_fn, Some(func_proto))?;
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
    set_data_prop(
      &mut scope,
      proto,
      "entries",
      Value::Object(entries_fn),
      true,
    )?;

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
    scope
      .heap_mut()
      .object_set_prototype(keys_fn, Some(func_proto))?;
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

    // @@toStringTag branding for platform object detection (`Object.prototype.toString.call(x)`).
    let tag_value = scope.alloc_string("Headers")?;
    scope.push_root(Value::String(tag_value))?;
    scope.define_property(
      proto,
      PropertyKey::from_symbol(realm.intrinsics().well_known_symbols().to_string_tag),
      data_desc(Value::String(tag_value), false),
    )?;

    // Define global.
    let key = alloc_key(&mut scope, "Headers")?;
    scope.define_property(global, key, data_desc(Value::Object(ctor), true))?;
    proto
  };

  // --- Request ---
  let request_proto = {
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
    scope
      .heap_mut()
      .object_set_prototype(ctor, Some(func_proto))?;
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
    scope
      .heap_mut()
      .object_set_prototype(clone_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "clone", Value::Object(clone_fn), true)?;

    let text_id = vm.register_native_call(request_text_native)?;
    let text_name = scope.alloc_string("text")?;
    scope.push_root(Value::String(text_name))?;
    let text_fn = scope.alloc_native_function(text_id, None, text_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(text_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "text", Value::Object(text_fn), true)?;

    let json_id = vm.register_native_call(request_json_native)?;
    let json_name = scope.alloc_string("json")?;
    scope.push_root(Value::String(json_name))?;
    let json_fn = scope.alloc_native_function(json_id, None, json_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(json_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "json", Value::Object(json_fn), true)?;

    let array_buffer_id = vm.register_native_call(request_array_buffer_native)?;
    let array_buffer_name = scope.alloc_string("arrayBuffer")?;
    scope.push_root(Value::String(array_buffer_name))?;
    let array_buffer_fn =
      scope.alloc_native_function(array_buffer_id, None, array_buffer_name, 0)?;
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

    let bytes_id = vm.register_native_call(request_bytes_native)?;
    let bytes_name = scope.alloc_string("bytes")?;
    scope.push_root(Value::String(bytes_name))?;
    let bytes_fn = scope.alloc_native_function(bytes_id, None, bytes_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(bytes_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "bytes", Value::Object(bytes_fn), true)?;

    let blob_id = vm.register_native_call(request_blob_native)?;
    let blob_name = scope.alloc_string("blob")?;
    scope.push_root(Value::String(blob_name))?;
    let blob_fn = scope.alloc_native_function(blob_id, None, blob_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(blob_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "blob", Value::Object(blob_fn), true)?;

    let form_data_id = vm.register_native_call(request_form_data_native)?;
    let form_data_name = scope.alloc_string("formData")?;
    scope.push_root(Value::String(form_data_name))?;
    let form_data_fn = scope.alloc_native_function(form_data_id, None, form_data_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(form_data_fn, Some(func_proto))?;
    set_data_prop(
      &mut scope,
      proto,
      "formData",
      Value::Object(form_data_fn),
      true,
    )?;

    // bodyUsed accessor (getter only).
    let body_used_get_id = vm.register_native_call(request_body_used_get_native)?;
    let body_used_get_name = scope.alloc_string("get bodyUsed")?;
    scope.push_root(Value::String(body_used_get_name))?;
    let body_used_get =
      scope.alloc_native_function(body_used_get_id, None, body_used_get_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(body_used_get, Some(func_proto))?;
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

    // @@toStringTag branding for platform object detection (`Object.prototype.toString.call(x)`).
    let tag_value = scope.alloc_string("Request")?;
    scope.push_root(Value::String(tag_value))?;
    scope.define_property(
      proto,
      PropertyKey::from_symbol(realm.intrinsics().well_known_symbols().to_string_tag),
      data_desc(Value::String(tag_value), false),
    )?;

    let key = alloc_key(&mut scope, "Request")?;
    scope.define_property(global, key, data_desc(Value::Object(ctor), true))?;
    proto
  };

  // Request.body accessor (getter only).
  {
    let body_get_id = vm.register_native_call(request_body_get_native)?;
    let body_get_name = scope.alloc_string("get body")?;
    scope.push_root(Value::String(body_get_name))?;
    let body_get = scope.alloc_native_function(body_get_id, None, body_get_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(body_get, Some(func_proto))?;
    // Root before allocating the property key: `alloc_key` can trigger GC.
    scope.push_root(Value::Object(body_get))?;
    let body_key = alloc_key(&mut scope, "body")?;
    scope.define_property(
      request_proto,
      body_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(body_get),
          set: Value::Undefined,
        },
      },
    )?;
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
    scope
      .heap_mut()
      .object_set_prototype(ctor, Some(func_proto))?;
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
    scope
      .heap_mut()
      .object_set_prototype(text_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "text", Value::Object(text_fn), true)?;

    let json_id = vm.register_native_call(response_json_native)?;
    let json_name = scope.alloc_string("json")?;
    scope.push_root(Value::String(json_name))?;
    let json_fn = scope.alloc_native_function(json_id, None, json_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(json_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "json", Value::Object(json_fn), true)?;

    let array_buffer_id = vm.register_native_call(response_array_buffer_native)?;
    let array_buffer_name = scope.alloc_string("arrayBuffer")?;
    scope.push_root(Value::String(array_buffer_name))?;
    let array_buffer_fn =
      scope.alloc_native_function(array_buffer_id, None, array_buffer_name, 0)?;
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

    let bytes_id = vm.register_native_call(response_bytes_native)?;
    let bytes_name = scope.alloc_string("bytes")?;
    scope.push_root(Value::String(bytes_name))?;
    let bytes_fn = scope.alloc_native_function(bytes_id, None, bytes_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(bytes_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "bytes", Value::Object(bytes_fn), true)?;

    let blob_id = vm.register_native_call(response_blob_native)?;
    let blob_name = scope.alloc_string("blob")?;
    scope.push_root(Value::String(blob_name))?;
    let blob_fn = scope.alloc_native_function(blob_id, None, blob_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(blob_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "blob", Value::Object(blob_fn), true)?;

    let form_data_id = vm.register_native_call(response_form_data_native)?;
    let form_data_name = scope.alloc_string("formData")?;
    scope.push_root(Value::String(form_data_name))?;
    let form_data_fn = scope.alloc_native_function(form_data_id, None, form_data_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(form_data_fn, Some(func_proto))?;
    set_data_prop(
      &mut scope,
      proto,
      "formData",
      Value::Object(form_data_fn),
      true,
    )?;

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
    scope
      .heap_mut()
      .object_set_prototype(clone_fn, Some(func_proto))?;
    set_data_prop(&mut scope, proto, "clone", Value::Object(clone_fn), true)?;

    // bodyUsed accessor (getter only).
    let body_used_get_id = vm.register_native_call(response_body_used_get_native)?;
    let body_used_get_name = scope.alloc_string("get bodyUsed")?;
    scope.push_root(Value::String(body_used_get_name))?;
    let body_used_get =
      scope.alloc_native_function(body_used_get_id, None, body_used_get_name, 0)?;
    scope
      .heap_mut()
      .object_set_prototype(body_used_get, Some(func_proto))?;
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

    // body accessor (getter only).
    let body_get_id = vm.register_native_call(response_body_get_native)?;
    let body_get_name = scope.alloc_string("get body")?;
    scope.push_root(Value::String(body_get_name))?;
    let body_get = scope.alloc_native_function(body_get_id, None, body_get_name, 0)?;
    scope.heap_mut().object_set_prototype(body_get, Some(func_proto))?;
    // Root before allocating the property key: `alloc_key` can trigger GC.
    scope.push_root(Value::Object(body_get))?;
    let body_key = alloc_key(&mut scope, "body")?;
    scope.define_property(
      proto,
      body_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(body_get),
          set: Value::Undefined,
        },
      },
    )?;

    // @@toStringTag branding for platform object detection (`Object.prototype.toString.call(x)`).
    let tag_value = scope.alloc_string("Response")?;
    scope.push_root(Value::String(tag_value))?;
    scope.define_property(
      proto,
      PropertyKey::from_symbol(realm.intrinsics().well_known_symbols().to_string_tag),
      data_desc(Value::String(tag_value), false),
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
    scope
      .heap_mut()
      .object_set_prototype(error_fn, Some(func_proto))?;
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
    scope
      .heap_mut()
      .object_set_prototype(redirect_fn, Some(func_proto))?;
    set_data_prop(
      &mut scope,
      ctor,
      "redirect",
      Value::Object(redirect_fn),
      true,
    )?;

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
    scope
      .heap_mut()
      .object_set_prototype(json_fn, Some(func_proto))?;
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
    scope
      .heap_mut()
      .object_set_prototype(func, Some(func_proto))?;
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
  use crate::clock::VirtualClock;
  use crate::js::event_loop::{
    EventLoop, RunLimits, RunNextTaskLimitedOutcome, RunUntilIdleOutcome, RunUntilIdleStopReason,
  };
  use crate::js::realm_module_loader::ModuleLoader;
  use crate::js::window::WindowHost;
  use crate::js::window_realm::WindowRealm;
  use crate::js::window_realm::WindowRealmConfig;
  use crate::js::JsExecutionOptions;
  use crate::resource::FetchedResource;
  use selectors::context::QuirksMode;
  use std::cell::RefCell;
  use std::collections::VecDeque;
  use std::rc::Rc;
  use std::sync::Arc;
  use std::time::Duration;
  use vm_js::PromiseState;
  use vm_js::{ExecutionContext, HeapLimits, RootId, VmOptions};
  use vm_js::{Job, RealmId, VmHostHooks};
  use webidl_vm_js::{host_from_hooks, WebIdlBindingsHost};

  // `vm-js` intrinsics + our window/fetch bindings have grown over time; keep unit tests resilient
  // to that by using a slightly larger heap than the historical 1MiB.
  const TEST_HEAP_BYTES: usize = 4 * 1024 * 1024;

  fn make_user_data(document_url: &str) -> WindowRealmUserData {
    let url = document_url.to_string();
    let module_loader = Rc::new(RefCell::new(ModuleLoader::new(Some(url.clone()))));
    let config = WindowRealmConfig::new(url.clone());
    WindowRealmUserData::new(
      url,
      module_loader,
      config.session_storage_namespace_id,
      None,
      config.web_storage_quota_utf16_bytes,
    )
  }

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  struct DummyHost;

  impl WindowRealmHost for DummyHost {
    fn vm_host_and_window_realm(
      &mut self,
    ) -> crate::error::Result<(&mut dyn vm_js::VmHost, &mut WindowRealm)> {
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

  struct StreamsTeardownGuard {
    realm_id: RealmId,
  }

  impl StreamsTeardownGuard {
    fn install(vm: &mut Vm, realm: &Realm, heap: &mut Heap) -> Result<Self, VmError> {
      window_streams::install_window_streams_bindings(vm, realm, heap)?;
      Ok(Self { realm_id: realm.id() })
    }
  }

  impl Drop for StreamsTeardownGuard {
    fn drop(&mut self) {
      window_streams::teardown_window_streams_bindings_for_realm(self.realm_id);
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

    fn host_enqueue_promise_job_fallible(
      &mut self,
      ctx: &mut dyn vm_js::VmJobContext,
      job: Job,
      realm: Option<RealmId>,
    ) -> Result<(), VmError> {
      if self.jobs.try_reserve(1).is_err() {
        job.discard(ctx);
        return Err(VmError::OutOfMemory);
      }
      self.jobs.push_back((job, realm));
      Ok(())
    }
  }

  fn get_global_prop(host: &mut WindowHost, name: &str) -> Value {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .expect("push root global");
    let key_s = scope.alloc_string(name).expect("alloc key");
    scope
      .push_root(Value::String(key_s))
      .expect("push root key");
    let key = PropertyKey::from_string(key_s);
    vm.get(&mut scope, global, key).expect("get global prop")
  }

  fn get_global_prop_utf8(host: &mut WindowHost, name: &str) -> Option<String> {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .expect("push root global");
    let key_s = scope.alloc_string(name).ok()?;
    scope.push_root(Value::String(key_s)).ok()?;
    let key = PropertyKey::from_string(key_s);
    let value = scope.heap().get(global, &key).ok()?;
    match value {
      Value::String(s) => Some(scope.heap().get_string(s).ok()?.to_utf8_lossy()),
      Value::Undefined => Some(String::new()),
      _ => None,
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
        let mut vm = self
          .vm
          .execution_context_guard(ExecutionContext {
            realm,
            script_or_module: None,
          })?;
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
        let mut vm = self
          .vm
          .execution_context_guard(ExecutionContext {
            realm,
            script_or_module: None,
          })?;
        vm.construct_with_host_and_hooks(
          self.host, &mut scope, host_hooks, callee, args, new_target,
        )
      } else {
        self.vm.construct_with_host_and_hooks(
          self.host, &mut scope, host_hooks, callee, args, new_target,
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
    reads: Vec<(bool, Option<Vec<u8>>)>,
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

  fn capture_promise_error_native(
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
        // Root `obj` while allocating property keys: `alloc_key` can trigger GC.
        let mut scope = scope.reborrow();
        scope.push_root(Value::Object(obj))?;

        let name_key = alloc_key(&mut scope, "name")?;
        let name_val = vm.get(&mut scope, obj, name_key)?;
        let name = get_string(scope.heap(), name_val);

        let message_key = alloc_key(&mut scope, "message")?;
        let message_val = vm.get(&mut scope, obj, message_key)?;
        let message = get_string(scope.heap(), message_val);

        format!("{name}:{message}")
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let _streams_guard = StreamsTeardownGuard::install(&mut vm, &realm, &mut heap)?;
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
  fn object_prototype_to_string_uses_fetch_to_string_tags() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let headers = window.exec_script("Object.prototype.toString.call(new Headers())")?;
    assert_eq!(get_string(window.heap(), headers), "[object Headers]");

    let request = window.exec_script(
      "Object.prototype.toString.call(new Request('https://example.invalid/'))",
    )?;
    assert_eq!(get_string(window.heap(), request), "[object Request]");

    let response = window.exec_script("Object.prototype.toString.call(new Response())")?;
    assert_eq!(get_string(window.heap(), response), "[object Response]");

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn request_referrer_parsing_and_getter_semantics() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    // Default is the internal "client" referrer state, exposed as "about:client".
    let v = window.exec_script("new Request('https://example.invalid/').referrer")?;
    assert_eq!(get_string(window.heap(), v), "about:client");

    // Empty string means explicit "no-referrer", exposed as empty string.
    let v =
      window.exec_script("new Request('https://example.invalid/', { referrer: '' }).referrer")?;
    assert_eq!(get_string(window.heap(), v), "");

    // about:client maps to the internal "client" state.
    let v = window.exec_script(
      "new Request('https://example.invalid/', { referrer: 'about:client' }).referrer",
    )?;
    assert_eq!(get_string(window.heap(), v), "about:client");

    // Cross-origin referrer falls back to the internal "client" state.
    let v = window.exec_script(
      "new Request('https://example.invalid/', { referrer: 'https://other.invalid/' }).referrer",
    )?;
    assert_eq!(get_string(window.heap(), v), "about:client");

    // Invalid referrer string throws TypeError.
    let v = window.exec_script(
      "(() => {\
         try { new Request('https://example.invalid/', { referrer: 'https://[::1' }); return false; }\
         catch (e) { return e instanceof TypeError; }\
       })()",
    )?;
    assert_eq!(v, Value::Bool(true));

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn request_init_window_allows_null() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let result = window.exec_script(
      "(() => { new Request('https://example.invalid/', { window: null }); return 'ok'; })()",
    )?;
    assert_eq!(get_string(window.heap(), result), "ok");

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn request_init_window_non_null_throws() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let result = window.exec_script(
      "(() => {\
         try { new Request('https://example.invalid/', { window: {} }); return { ok: true }; }\
         catch (e) { return { ok: false, name: e.name, message: e.message }; }\
       })()",
    )?;
    let Value::Object(result_obj) = result else {
      return Err(VmError::InvariantViolation(
        "expected RequestInit.window error test script to return an object",
      ));
    };

    {
      let (vm, _realm, heap) = window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(result_obj))?;

      let ok_key = alloc_key(&mut scope, "ok")?;
      assert_eq!(vm.get(&mut scope, result_obj, ok_key)?, Value::Bool(false));

      let name_key = alloc_key(&mut scope, "name")?;
      let name_val = vm.get(&mut scope, result_obj, name_key)?;
      assert_eq!(get_string(scope.heap(), name_val), "TypeError");

      let message_key = alloc_key(&mut scope, "message")?;
      let message_val = vm.get(&mut scope, result_obj, message_key)?;
      assert_eq!(
        get_string(scope.heap(), message_val),
        "RequestInit.window must be null"
      );
    }

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn fetch_init_window_non_null_rejects_or_throws() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());
    let env = WindowFetchEnv::for_document(
      Arc::new(StaticOkFetcher),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            globalThis.__fetch_window_error_name = "";
            globalThis.__fetch_window_error_message = "";
            (async () => {
              try {
                await fetch("https://example.invalid/", { window: {} });
                globalThis.__fetch_window_error_name = "no error";
              } catch (e) {
                globalThis.__fetch_window_error_name = e && e.name ? e.name : String(e);
                globalThis.__fetch_window_error_message = e && e.message ? e.message : "";
              }
            })();
          "#,
        )
        .unwrap();
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let name_val = host
      .window
      .exec_script("globalThis.__fetch_window_error_name")
      .unwrap();
    assert_eq!(get_string(host.window.heap(), name_val), "TypeError");

    let message_val = host
      .window
      .exec_script("globalThis.__fetch_window_error_message")
      .unwrap();
    assert_eq!(
      get_string(host.window.heap(), message_val),
      "RequestInit.window must be null"
    );

    Ok(())
  }

  #[test]
  fn structured_clone_rejects_fetch_wrappers() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let headers = window.exec_script(
      "(() => {\
         try { structuredClone(new Headers()); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(window.heap(), headers), "DataCloneError");

    let headers_iter = window.exec_script(
      "(() => {\
         try { structuredClone(new Headers().entries()); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(window.heap(), headers_iter), "DataCloneError");

    let request = window.exec_script(
      "(() => {\
         try { structuredClone(new Request('/')); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(window.heap(), request), "DataCloneError");

    let response = window.exec_script(
      "(() => {\
         try { structuredClone(new Response('x')); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(window.heap(), response), "DataCloneError");

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn response_string_body_infers_text_plain_content_type() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let ct_val = window.exec_script("new Response('hi').headers.get('content-type')")?;
    assert_eq!(get_string(window.heap(), ct_val), "text/plain;charset=UTF-8");

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn request_string_body_does_not_override_existing_content_type() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let ct_val = window.exec_script(
      "new Request('https://example.invalid/', { method: 'POST', body: 'hi', headers: { 'Content-Type': 'application/custom' } }).headers.get('content-type')",
    )?;
    assert_eq!(get_string(window.heap(), ct_val), "application/custom");

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn request_exposes_cache_integrity_keepalive_and_navigation_flags() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let destination = window.exec_script("new Request('https://example.invalid/').destination")?;
    assert_eq!(get_string(window.heap(), destination), "");

    let cache = window.exec_script("new Request('https://example.invalid/').cache")?;
    assert_eq!(get_string(window.heap(), cache), "default");

    let integrity = window.exec_script("new Request('https://example.invalid/').integrity")?;
    assert_eq!(get_string(window.heap(), integrity), "");

    let keepalive = window.exec_script("new Request('https://example.invalid/').keepalive")?;
    assert!(matches!(keepalive, Value::Bool(false)));

    let is_reload_navigation =
      window.exec_script("new Request('https://example.invalid/').isReloadNavigation")?;
    assert!(matches!(is_reload_navigation, Value::Bool(false)));

    let is_history_navigation =
      window.exec_script("new Request('https://example.invalid/').isHistoryNavigation")?;
    assert!(matches!(is_history_navigation, Value::Bool(false)));

    let overridden_cache = window.exec_script(
      "new Request('https://example.invalid/', { cache: 'no-cache' }).cache",
    )?;
    assert_eq!(get_string(window.heap(), overridden_cache), "no-cache");

    let invalid_cache = window.exec_script(
      "(() => {\
         try { new Request('https://example.invalid/', { cache: 'bad' }); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(window.heap(), invalid_cache), "TypeError");

    let mode_navigate = window.exec_script(
      "(() => {\
         try { new Request('https://example.invalid/', { mode: 'navigate' }); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(window.heap(), mode_navigate), "TypeError");

    let only_if_cached_wrong_mode = window.exec_script(
      "(() => {\
         try { new Request('https://example.invalid/', { cache: 'only-if-cached', mode: 'cors' }); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(
      get_string(window.heap(), only_if_cached_wrong_mode),
      "TypeError"
    );

    let clone_preserves = window.exec_script(
      "(() => {\
         const r1 = new Request('https://example.invalid/', { cache:'reload', integrity:'x', keepalive:true });\
         const r2 = r1.clone();\
         return r2.cache === r1.cache && r2.integrity === r1.integrity && r2.keepalive === r1.keepalive;\
       })()",
    )?;
    assert!(matches!(clone_preserves, Value::Bool(true)));

    let new_from_existing_overrides_cache = window.exec_script(
      "(() => {\
         const r1 = new Request('https://example.invalid/', { cache:'reload', integrity:'x', keepalive:true });\
         const r2 = new Request(r1, { cache:'no-cache' });\
         return r2.cache === 'no-cache' && r2.integrity === 'x' && r2.keepalive === true;\
       })()",
    )?;
    assert!(matches!(new_from_existing_overrides_cache, Value::Bool(true)));

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn fetch_validates_request_init_cache() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let invalid_cache = window.exec_script(
      "(() => {\
         try { fetch('https://example.invalid/', { cache: 'bad' }); return 'no'; }\
         catch (e) { return e.name; }\
       })()",
    )?;
    assert_eq!(get_string(window.heap(), invalid_cache), "TypeError");

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn fetch_init_mode_navigate_throws_or_rejects_type_error() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let env = WindowFetchEnv::for_document(
      Arc::new(StaticOkFetcher),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            globalThis.__err = null;
            (async () => {
              try {
                await fetch('https://example.invalid/', { mode: 'navigate' });
              } catch (e) {
                globalThis.__err = e && e.name ? e.name : String(e);
              }
            })();
          "#,
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let err_val = host.window.exec_script("globalThis.__err").unwrap();
    assert!(
      matches!(err_val, Value::String(_)),
      "expected fetch to throw/reject TypeError, got {err_val:?}"
    );
    assert_eq!(get_string(host.window.heap(), err_val), "TypeError");
    Ok(())
  }

  #[test]
  fn request_signal_is_non_null_abort_signal() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let ok = window.exec_script(
      "(() => {\
         const r = new Request('https://example.invalid/');\
         return typeof r.signal === 'object' && r.signal !== null && r.signal.aborted === false;\
       })()",
    )?;
    assert!(matches!(ok, Value::Bool(true)), "got {ok:?}");

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn request_signal_is_dependent_on_init_signal() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let ok = window.exec_script(
      "(() => {\
         const c = new AbortController();\
         const r = new Request('https://example.invalid/', { signal: c.signal });\
         if (r.signal === c.signal) return false;\
         c.abort('x');\
         return r.signal.aborted === true && r.signal.reason === 'x';\
       })()",
    )?;
    assert!(matches!(ok, Value::Bool(true)), "got {ok:?}");

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn request_clone_creates_dependent_signal() -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let ok = window.exec_script(
      "(() => {\
         const r1 = new Request('https://example.invalid/');\
         const r2 = r1.clone();\
         return r2.signal !== r1.signal;\
       })()",
    )?;
    assert!(matches!(ok, Value::Bool(true)), "got {ok:?}");

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn fetch_does_not_override_global_readable_stream_when_window_streams_installed(
  ) -> Result<(), VmError> {
    let mut window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))?;

    // WindowRealm installs `window_streams` bindings by default. Fetch must not replace the global
    // `ReadableStream` constructor with its own implementation.
    window.exec_script("globalThis.__rs_ctor_before_fetch = ReadableStream")?;
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
      )?
    };

    let stream_ctor_is_unchanged =
      window.exec_script("globalThis.__rs_ctor_before_fetch === ReadableStream")?;
    assert!(
      matches!(stream_ctor_is_unchanged, Value::Bool(true)),
      "expected fetch to not override global ReadableStream, got {stream_ctor_is_unchanged:?}"
    );

    let stream_env_id = window.exec_script("new ReadableStream().__fastrender_fetch_env_id")?;
    assert!(
      matches!(stream_env_id, Value::Undefined),
      "expected global ReadableStream to come from window_streams, got {stream_env_id:?}"
    );

    let response_body_is_global_readable_stream =
      window.exec_script("new Response('x').body instanceof ReadableStream")?;
    assert!(
      matches!(response_body_is_global_readable_stream, Value::Bool(true)),
      "expected Response.body to use global ReadableStream brand, got {response_body_is_global_readable_stream:?}"
    );

    let has_get_reader = window.exec_script("typeof new Response('x').body.getReader === 'function'")?;
    assert!(
      matches!(has_get_reader, Value::Bool(true)),
      "expected Response.body.getReader to be a function, got {has_get_reader:?}"
    );

    drop(fetch_bindings);
    window.teardown();
    Ok(())
  }

  #[test]
  fn gc_sweeps_unreferenced_responses() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_heap_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024)),
    )?;

    let env = WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None);
    let bindings = {
      let (vm, vm_realm, heap) = realm.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(vm, vm_realm, heap, env)?
    };
    let env_id = bindings.env_id();

    let baseline = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let state = lock
        .get(&env_id)
        .ok_or(VmError::InvariantViolation("fetch env state missing"))?;
      state.responses.len()
    };

    realm.exec_script("for (let i = 0; i < 50; i++) new Response('x');")?;

    let before_gc = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let state = lock
        .get(&env_id)
        .ok_or(VmError::InvariantViolation("fetch env state missing"))?;
      state.responses.len()
    };
    assert!(
      before_gc >= baseline + 50,
      "expected responses to grow before GC; baseline={baseline} before_gc={before_gc}"
    );

    realm.heap_mut().collect_garbage();
    sweep_env_state_if_gc_ran(env_id, realm.heap())?;

    let after_gc = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let state = lock
        .get(&env_id)
        .ok_or(VmError::InvariantViolation("fetch env state missing"))?;
      state.responses.len()
    };
    assert_eq!(
      after_gc, baseline,
      "expected responses to be swept after GC; baseline={baseline} after_gc={after_gc}"
    );

    drop(bindings);
    realm.teardown();
    Ok(())
  }

  #[test]
  fn gc_sweeps_unreferenced_owned_headers() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_heap_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024)),
    )?;

    let env = WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None);
    let bindings = {
      let (vm, vm_realm, heap) = realm.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(vm, vm_realm, heap, env)?
    };
    let env_id = bindings.env_id();

    let baseline = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let state = lock
        .get(&env_id)
        .ok_or(VmError::InvariantViolation("fetch env state missing"))?;
      state.owned_headers.len()
    };

    realm.exec_script("for (let i = 0; i < 50; i++) new Headers({ a: 'b' });")?;

    let before_gc = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let state = lock
        .get(&env_id)
        .ok_or(VmError::InvariantViolation("fetch env state missing"))?;
      state.owned_headers.len()
    };
    assert!(
      before_gc >= baseline + 50,
      "expected owned headers to grow before GC; baseline={baseline} before_gc={before_gc}"
    );

    realm.heap_mut().collect_garbage();
    sweep_env_state_if_gc_ran(env_id, realm.heap())?;

    let after_gc = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let state = lock
        .get(&env_id)
        .ok_or(VmError::InvariantViolation("fetch env state missing"))?;
      state.owned_headers.len()
    };
    assert_eq!(
      after_gc, baseline,
      "expected owned headers to be swept after GC; baseline={baseline} after_gc={after_gc}"
    );

    drop(bindings);
    realm.teardown();
    Ok(())
  }

  #[test]
  fn response_body_used_getter_rejects_invalid_this() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
  fn response_body_getter_returns_stream_or_null() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let _streams_guard = StreamsTeardownGuard::install(&mut vm, &realm, &mut heap)?;

    let _bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
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

    // new Response('hi').body is a ReadableStream object.
    let hi_s = scope.alloc_string("hi")?;
    scope.push_root(Value::String(hi_s))?;
    let Value::Object(resp_obj) = response_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      response_ctor,
      &[Value::String(hi_s)],
      Value::Object(response_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation("Response constructor must return an object"));
    };

    let body_key = alloc_key(&mut scope, "body")?;
    let body1 = vm.get(&mut scope, resp_obj, body_key)?;
    assert!(matches!(body1, Value::Object(_)));
    // Cached per Response instance.
    let body2 = vm.get(&mut scope, resp_obj, body_key)?;
    assert_eq!(body1, body2);

    // Accessing `.body` should not disturb cloning.
    let clone_key = alloc_key(&mut scope, "clone")?;
    let clone_fn = vm.get(&mut scope, resp_obj, clone_key)?;
    let cloned = vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      clone_fn,
      Value::Object(resp_obj),
      &[],
    )?;
    assert!(
      matches!(cloned, Value::Object(_)),
      "Response.clone must return an object"
    );

    // new Response().body === null.
    let Value::Object(resp_null_obj) = response_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      response_ctor,
      &[],
      Value::Object(response_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation("Response constructor must return an object"));
    };
    let body_null = vm.get(&mut scope, resp_null_obj, body_key)?;
    assert!(matches!(body_null, Value::Null));

    drop(scope);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn readable_stream_read_consumes_response_body_and_unlocks() -> Result<(), VmError> {
    #[derive(Default)]
    struct HostState {
      reads: Vec<(bool, Option<Vec<u8>>)>,
    }

    fn capture_read_result_native(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn vm_js::VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      _this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      let Value::Object(result_obj) = args.get(0).copied().unwrap_or(Value::Undefined) else {
        return Err(VmError::TypeError("expected { value, done } object"));
      };
      scope.push_root(Value::Object(result_obj))?;
      let done_key = alloc_key(scope, "done")?;
      let value_key = alloc_key(scope, "value")?;

      let done_val = vm.get(scope, result_obj, done_key)?;
      let done = matches!(done_val, Value::Bool(true));

      let value_val = vm.get(scope, result_obj, value_key)?;
      let bytes = match value_val {
        Value::Undefined => None,
        Value::Object(obj) => {
          if !scope.heap().is_uint8_array_object(obj) {
            return Err(VmError::TypeError("expected Uint8Array value"));
          }
          Some(scope.heap().uint8_array_data(obj)?.to_vec())
        }
        _ => return Err(VmError::TypeError("expected Uint8Array value")),
      };

      let state = host
        .as_any_mut()
        .downcast_mut::<HostState>()
        .ok_or(VmError::InvariantViolation("unexpected host state type"))?;
      state.reads.push((done, bytes));
      Ok(Value::Undefined)
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let _streams_guard = StreamsTeardownGuard::install(&mut vm, &realm, &mut heap)?;

    let _bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;

    let mut host_state = HostState::default();
    let mut hooks = JobQueueHooks::default();

    let (resp_obj, body_obj, reader_obj) = {
      let mut scope = heap.scope();
      let global = realm.global_object();
      let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
        return Err(VmError::InvariantViolation("Response constructor missing"));
      };

      let hi_s = scope.alloc_string("hi")?;
      scope.push_root(Value::String(hi_s))?;
      let Value::Object(resp_obj) = response_ctor_construct(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        response_ctor,
        &[Value::String(hi_s)],
        Value::Object(response_ctor),
      )?
      else {
        return Err(VmError::InvariantViolation("Response constructor must return an object"));
      };

      let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
      let used_before = vm.get(&mut scope, resp_obj, body_used_key)?;
      assert!(matches!(used_before, Value::Bool(false)));

      let body_key = alloc_key(&mut scope, "body")?;
      let Value::Object(body_obj) = vm.get(&mut scope, resp_obj, body_key)? else {
        return Err(VmError::InvariantViolation("Response.body must return an object"));
      };

      let locked_key = alloc_key(&mut scope, "locked")?;
      let locked_before = vm.get(&mut scope, body_obj, locked_key)?;
      assert!(matches!(locked_before, Value::Bool(false)));

      let get_reader_key = alloc_key(&mut scope, "getReader")?;
      let get_reader_fn = vm.get(&mut scope, body_obj, get_reader_key)?;
      let Value::Object(reader_obj) = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        get_reader_fn,
        Value::Object(body_obj),
        &[],
      )?
      else {
        return Err(VmError::InvariantViolation("ReadableStream.getReader must return an object"));
      };

      let locked_after = vm.get(&mut scope, body_obj, locked_key)?;
      assert!(matches!(locked_after, Value::Bool(true)));

      (resp_obj, body_obj, reader_obj)
    };

    let resp_root = heap.add_root(Value::Object(resp_obj))?;
    let body_root = heap.add_root(Value::Object(body_obj))?;
    let reader_root = heap.add_root(Value::Object(reader_obj))?;

    let capture_id = vm.register_native_call(capture_read_result_native)?;
    let func_proto = realm.intrinsics().function_prototype();

    // reader.read() -> { done:false, value: Uint8Array([104, 105]) }
    {
      let mut scope = heap.scope();
      let read_key = alloc_key(&mut scope, "read")?;
      let read_fn = vm.get(&mut scope, reader_obj, read_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        read_fn,
        Value::Object(reader_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("read() must return a Promise object"));
      };

      let on_fulfilled = {
        let name = scope.alloc_string("onFulfilled")?;
        scope.push_root(Value::String(name))?;
        let f = scope.alloc_native_function(capture_id, None, name, 1)?;
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
        &[Value::Object(on_fulfilled), Value::Undefined],
      )?;

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
    }

    assert_eq!(host_state.reads.len(), 1);
    assert_eq!(host_state.reads[0].0, false);
    assert_eq!(
      host_state.reads[0].1.as_deref(),
      Some(&[104u8, 105u8][..])
    );

    // Response.bodyUsed flips to true after the first read.
    {
      let mut scope = heap.scope();
      let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
      let used_after = vm.get(&mut scope, resp_obj, body_used_key)?;
      assert!(matches!(used_after, Value::Bool(true)));
    }

    // Second read: done:true, value: undefined.
    {
      let mut scope = heap.scope();
      let read_key = alloc_key(&mut scope, "read")?;
      let read_fn = vm.get(&mut scope, reader_obj, read_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        read_fn,
        Value::Object(reader_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("read() must return a Promise object"));
      };

      let on_fulfilled = {
        let name = scope.alloc_string("onFulfilled2")?;
        scope.push_root(Value::String(name))?;
        let f = scope.alloc_native_function(capture_id, None, name, 1)?;
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
        &[Value::Object(on_fulfilled), Value::Undefined],
      )?;

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
    }

    assert_eq!(host_state.reads.len(), 2);
    assert_eq!(host_state.reads[1].0, true);
    assert!(host_state.reads[1].1.is_none());

    // releaseLock() unlocks the stream.
    {
      let mut scope = heap.scope();
      let release_key = alloc_key(&mut scope, "releaseLock")?;
      let release_fn = vm.get(&mut scope, reader_obj, release_key)?;
      vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        release_fn,
        Value::Object(reader_obj),
        &[],
      )?;

      let locked_key = alloc_key(&mut scope, "locked")?;
      let locked_after = vm.get(&mut scope, body_obj, locked_key)?;
      assert!(matches!(locked_after, Value::Bool(false)));

      // Further reads reject with TypeError.
      let read_key = alloc_key(&mut scope, "read")?;
      let read_fn = vm.get(&mut scope, reader_obj, read_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        read_fn,
        Value::Object(reader_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("read() must return a Promise object"));
      };
      assert_eq!(
        scope.heap().promise_state(promise_obj)?,
        PromiseState::Rejected,
        "expected read() after releaseLock to return a rejected Promise"
      );
      let Some(reason) = scope.heap().promise_result(promise_obj)? else {
        return Err(VmError::InvariantViolation(
          "read() rejected Promise missing reason",
        ));
      };
      let Value::Object(err_obj) = reason else {
        return Err(VmError::InvariantViolation(
          "read() rejection reason must be an object",
        ));
      };
      scope.push_root(Value::Object(err_obj))?;
      let message_key = alloc_key(&mut scope, "message")?;
      let message_val = vm.get(&mut scope, err_obj, message_key)?;
      let Value::String(message_str) = message_val else {
        return Err(VmError::InvariantViolation(
          "read() rejection reason must have a message string",
        ));
      };
      let msg = scope.heap().get_string(message_str)?.to_utf8_lossy();
      assert!(
        msg.starts_with("ReadableStreamDefaultReader has no stream"),
        "expected TypeError message to mention missing stream, got {msg:?}"
      );
    }

    heap.remove_root(resp_root);
    heap.remove_root(body_root);
    heap.remove_root(reader_root);

    drop(hooks);
    drop(host_state);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_text_rejects_when_body_used_by_body_stream() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let _streams_guard = StreamsTeardownGuard::install(&mut vm, &realm, &mut heap)?;

    let _bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;

    let mut host_state = CaptureHostState::default();
    let mut hooks = JobQueueHooks::default();

    let resp_obj = {
      let mut scope = heap.scope();
      let global = realm.global_object();
      let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
        return Err(VmError::InvariantViolation("Response constructor missing"));
      };

      let hi_s = scope.alloc_string("hi")?;
      scope.push_root(Value::String(hi_s))?;
      let Value::Object(resp_obj) = response_ctor_construct(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        response_ctor,
        &[Value::String(hi_s)],
        Value::Object(response_ctor),
      )?
      else {
        return Err(VmError::InvariantViolation("Response constructor must return an object"));
      };
      // Root response/body/reader across subsequent allocations: `alloc_key` can trigger GC.
      scope.push_root(Value::Object(resp_obj))?;

      // Consume via body stream.
      let body_key = alloc_key(&mut scope, "body")?;
      let Value::Object(body_obj) = vm.get(&mut scope, resp_obj, body_key)? else {
        return Err(VmError::InvariantViolation("Response.body must return an object"));
      };
      scope.push_root(Value::Object(body_obj))?;
      let get_reader_key = alloc_key(&mut scope, "getReader")?;
      let get_reader_fn = vm.get(&mut scope, body_obj, get_reader_key)?;
      let Value::Object(reader_obj) = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        get_reader_fn,
        Value::Object(body_obj),
        &[],
      )?
      else {
        return Err(VmError::InvariantViolation("ReadableStream.getReader must return an object"));
      };
      scope.push_root(Value::Object(reader_obj))?;

      let read_key = alloc_key(&mut scope, "read")?;
      let read_fn = vm.get(&mut scope, reader_obj, read_key)?;
      let _ = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        read_fn,
        Value::Object(reader_obj),
        &[],
      )?;

      let release_key = alloc_key(&mut scope, "releaseLock")?;
      let release_fn = vm.get(&mut scope, reader_obj, release_key)?;
      vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        release_fn,
        Value::Object(reader_obj),
        &[],
      )?;

      resp_obj
    };

    let resp_root = heap.add_root(Value::Object(resp_obj))?;

    // Response.bodyUsed flips to true after consumption via stream read.
    {
      let mut scope = heap.scope();
      let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
      let used = vm.get(&mut scope, resp_obj, body_used_key)?;
      assert!(matches!(used, Value::Bool(true)));
    }

    // Response.text() rejects with BodyUsed (not locked) after releaseLock.
    host_state.fulfilled = None;
    host_state.rejected = None;

    {
      let mut scope = heap.scope();
      let text_key = alloc_key(&mut scope, "text")?;
      let text_fn = vm.get(&mut scope, resp_obj, text_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        text_fn,
        Value::Object(resp_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("Response.text must return a Promise object"));
      };

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

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      let on_rejected_root = scope.heap_mut().add_root(Value::Object(on_rejected))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
      heap.remove_root(on_rejected_root);
    }

    let rejected = host_state.rejected.clone().unwrap_or_default();
    assert!(
      rejected.contains("body is already used"),
      "expected rejection to mention BodyUsed, got {rejected:?}"
    );

    heap.remove_root(resp_root);
    drop(hooks);
    drop(host_state);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn response_consumers_reject_when_body_stream_locked() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let _streams_guard = StreamsTeardownGuard::install(&mut vm, &realm, &mut heap)?;

    let _bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;

    let mut host_state = CaptureHostState::default();
    let mut hooks = JobQueueHooks::default();

    let (resp_obj, body_obj, reader_obj) = {
      let mut scope = heap.scope();
      let global = realm.global_object();
      let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
        return Err(VmError::InvariantViolation("Response constructor missing"));
      };

      let hi_s = scope.alloc_string("hi")?;
      scope.push_root(Value::String(hi_s))?;
      let Value::Object(resp_obj) = response_ctor_construct(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        response_ctor,
        &[Value::String(hi_s)],
        Value::Object(response_ctor),
      )?
      else {
        return Err(VmError::InvariantViolation("Response constructor must return an object"));
      };

      let body_key = alloc_key(&mut scope, "body")?;
      let Value::Object(body_obj) = vm.get(&mut scope, resp_obj, body_key)? else {
        return Err(VmError::InvariantViolation("Response.body must return an object"));
      };

      let get_reader_key = alloc_key(&mut scope, "getReader")?;
      let get_reader_fn = vm.get(&mut scope, body_obj, get_reader_key)?;
      let Value::Object(reader_obj) = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        get_reader_fn,
        Value::Object(body_obj),
        &[],
      )?
      else {
        return Err(VmError::InvariantViolation("ReadableStream.getReader must return an object"));
      };

      (resp_obj, body_obj, reader_obj)
    };

    let resp_root = heap.add_root(Value::Object(resp_obj))?;
    let body_root = heap.add_root(Value::Object(body_obj))?;
    let reader_root = heap.add_root(Value::Object(reader_obj))?;

    // resp.text() rejects with TypeError when locked.
    {
      let mut scope = heap.scope();
      let text_key = alloc_key(&mut scope, "text")?;
      let text_fn = vm.get(&mut scope, resp_obj, text_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        text_fn,
        Value::Object(resp_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("Response.text must return a Promise object"));
      };

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

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      let on_rejected_root = scope.heap_mut().add_root(Value::Object(on_rejected))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
      heap.remove_root(on_rejected_root);
    }

    assert_eq!(host_state.fulfilled, None);
    assert_eq!(host_state.rejected.as_deref(), Some("Response body is locked"));

    // resp.clone() throws TypeError when locked.
    {
      let mut scope = heap.scope();
      let clone_key = alloc_key(&mut scope, "clone")?;
      let clone_fn = vm.get(&mut scope, resp_obj, clone_key)?;
      let err = vm
        .call_with_host_and_hooks(
          &mut host_state,
          &mut scope,
          &mut hooks,
          clone_fn,
          Value::Object(resp_obj),
          &[],
        )
        .expect_err("expected Response.clone to throw while locked");

      match err {
        VmError::TypeError(msg) => assert_eq!(msg, "Response body is locked"),
        other => {
          let Some(Value::Object(err_obj)) = other.thrown_value() else {
            return Err(VmError::InvariantViolation("expected thrown TypeError"));
          };
          scope.push_root(Value::Object(err_obj))?;
          let message_key = alloc_key(&mut scope, "message")?;
          let message_val = vm.get(&mut scope, err_obj, message_key)?;
          let Value::String(message_str) = message_val else {
            return Err(VmError::InvariantViolation("expected TypeError.message string"));
          };
          let msg = scope.heap().get_string(message_str)?.to_utf8_lossy().to_string();
          assert_eq!(msg, "Response body is locked");
        }
      }
    }

    heap.remove_root(resp_root);
    heap.remove_root(body_root);
    heap.remove_root(reader_root);
    drop(hooks);
    drop(host_state);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn readable_stream_read_consumes_request_body_and_marks_body_used() -> Result<(), VmError> {
    fn capture_read_result_native(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn vm_js::VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      _this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      let Value::Object(result_obj) = args.get(0).copied().unwrap_or(Value::Undefined) else {
        return Err(VmError::TypeError("expected { value, done } object"));
      };
      scope.push_root(Value::Object(result_obj))?;
      let done_key = alloc_key(scope, "done")?;
      let value_key = alloc_key(scope, "value")?;

      let done_val = vm.get(scope, result_obj, done_key)?;
      let done = matches!(done_val, Value::Bool(true));

      let value_val = vm.get(scope, result_obj, value_key)?;
      let bytes = match value_val {
        Value::Undefined => None,
        Value::Object(obj) => {
          if !scope.heap().is_uint8_array_object(obj) {
            return Err(VmError::TypeError("expected Uint8Array value"));
          }
          Some(scope.heap().uint8_array_data(obj)?.to_vec())
        }
        _ => return Err(VmError::TypeError("expected Uint8Array value")),
      };

      let state = host
        .as_any_mut()
        .downcast_mut::<CaptureHostState>()
        .ok_or(VmError::InvariantViolation("unexpected host state type"))?;
      state.reads.push((done, bytes));
      Ok(Value::Undefined)
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let _streams_guard = StreamsTeardownGuard::install(&mut vm, &realm, &mut heap)?;

    let _bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;

    let mut host_state = CaptureHostState::default();
    let mut hooks = JobQueueHooks::default();

    let (req_obj, body_obj, reader_obj) = {
      let mut scope = heap.scope();
      let global = realm.global_object();
      let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
        return Err(VmError::InvariantViolation("Request constructor missing"));
      };

      let url_s = scope.alloc_string("https://example.com/")?;
      scope.push_root(Value::String(url_s))?;

      // new Request(url, { method: "POST", body: "hi" })
      let init_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(init_obj))?;
      let method_s = scope.alloc_string("POST")?;
      scope.push_root(Value::String(method_s))?;
      let body_s = scope.alloc_string("hi")?;
      scope.push_root(Value::String(body_s))?;
      set_data_prop(
        &mut scope,
        init_obj,
        "method",
        Value::String(method_s),
        true,
      )?;
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

      let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
      let used_before = vm.get(&mut scope, req_obj, body_used_key)?;
      assert!(matches!(used_before, Value::Bool(false)));

      let body_key = alloc_key(&mut scope, "body")?;
      let body1 = vm.get(&mut scope, req_obj, body_key)?;
      assert!(matches!(body1, Value::Object(_)));
      // Cached per Request instance.
      let body2 = vm.get(&mut scope, req_obj, body_key)?;
      assert_eq!(body1, body2);
      let Value::Object(body_obj) = body1 else {
        return Err(VmError::InvariantViolation("Request.body must return an object"));
      };

      // Accessing `.body` should not flip `bodyUsed`.
      let used_after_access = vm.get(&mut scope, req_obj, body_used_key)?;
      assert!(matches!(used_after_access, Value::Bool(false)));

      // Accessing `.body` should not prevent cloning.
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
      assert!(
        matches!(cloned, Value::Object(_)),
        "Request.clone must return an object"
      );

      let get_reader_key = alloc_key(&mut scope, "getReader")?;
      let get_reader_fn = vm.get(&mut scope, body_obj, get_reader_key)?;
      let Value::Object(reader_obj) = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        get_reader_fn,
        Value::Object(body_obj),
        &[],
      )?
      else {
        return Err(VmError::InvariantViolation("ReadableStream.getReader must return an object"));
      };

      (req_obj, body_obj, reader_obj)
    };

    let req_root = heap.add_root(Value::Object(req_obj))?;
    let body_root = heap.add_root(Value::Object(body_obj))?;
    let reader_root = heap.add_root(Value::Object(reader_obj))?;

    let capture_id = vm.register_native_call(capture_read_result_native)?;
    let func_proto = realm.intrinsics().function_prototype();

    // reader.read() -> { done:false, value: Uint8Array([104, 105]) }
    {
      let mut scope = heap.scope();
      let read_key = alloc_key(&mut scope, "read")?;
      let read_fn = vm.get(&mut scope, reader_obj, read_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        read_fn,
        Value::Object(reader_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("read() must return a Promise object"));
      };

      let on_fulfilled = {
        let name = scope.alloc_string("onFulfilled")?;
        scope.push_root(Value::String(name))?;
        let f = scope.alloc_native_function(capture_id, None, name, 1)?;
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
        &[Value::Object(on_fulfilled), Value::Undefined],
      )?;

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
    }

    assert_eq!(host_state.reads.len(), 1);
    assert_eq!(host_state.reads[0].0, false);
    assert_eq!(
      host_state.reads[0].1.as_deref(),
      Some(&[104u8, 105u8][..])
    );

    // Request.bodyUsed flips to true after the first read.
    {
      let mut scope = heap.scope();
      let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
      let used_after = vm.get(&mut scope, req_obj, body_used_key)?;
      assert!(matches!(used_after, Value::Bool(true)));
    }

    // Second read: done:true, value: undefined.
    {
      let mut scope = heap.scope();
      let read_key = alloc_key(&mut scope, "read")?;
      let read_fn = vm.get(&mut scope, reader_obj, read_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        read_fn,
        Value::Object(reader_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("read() must return a Promise object"));
      };

      let on_fulfilled = {
        let name = scope.alloc_string("onFulfilled2")?;
        scope.push_root(Value::String(name))?;
        let f = scope.alloc_native_function(capture_id, None, name, 1)?;
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
        &[Value::Object(on_fulfilled), Value::Undefined],
      )?;

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
    }

    assert_eq!(host_state.reads.len(), 2);
    assert_eq!(host_state.reads[1].0, true);
    assert!(host_state.reads[1].1.is_none());

    // releaseLock() unlocks the stream, letting body methods reject with BodyUsed instead of locked.
    {
      let mut scope = heap.scope();
      let release_key = alloc_key(&mut scope, "releaseLock")?;
      let release_fn = vm.get(&mut scope, reader_obj, release_key)?;
      vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        release_fn,
        Value::Object(reader_obj),
        &[],
      )?;

      let locked_key = alloc_key(&mut scope, "locked")?;
      let locked_after = vm.get(&mut scope, body_obj, locked_key)?;
      assert!(matches!(locked_after, Value::Bool(false)));
    }

    // req.text() rejects with TypeError mentioning BodyUsed.
    host_state.fulfilled = None;
    host_state.rejected = None;
    {
      let mut scope = heap.scope();
      let text_key = alloc_key(&mut scope, "text")?;
      let text_fn = vm.get(&mut scope, req_obj, text_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        text_fn,
        Value::Object(req_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("Request.text must return a Promise object"));
      };

      let capture_id = vm.register_native_call(capture_promise_string_native)?;
      let func_proto = realm.intrinsics().function_prototype();
      let on_fulfilled = {
        let name = scope.alloc_string("onFulfilledText")?;
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
        let name = scope.alloc_string("onRejectedText")?;
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

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      let on_rejected_root = scope.heap_mut().add_root(Value::Object(on_rejected))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
      heap.remove_root(on_rejected_root);
    }

    let rejected = host_state.rejected.clone().unwrap_or_default();
    assert!(
      rejected.contains("body is already used"),
      "expected rejection to mention BodyUsed, got {rejected:?}"
    );

    heap.remove_root(req_root);
    heap.remove_root(body_root);
    heap.remove_root(reader_root);

    drop(hooks);
    drop(host_state);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn gc_sweep_keeps_request_backing_alive_while_body_stream_alive() -> Result<(), VmError> {
    fn capture_read_result_native(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn vm_js::VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      _this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      let Value::Object(result_obj) = args.get(0).copied().unwrap_or(Value::Undefined) else {
        return Err(VmError::TypeError("expected { value, done } object"));
      };
      scope.push_root(Value::Object(result_obj))?;
      let done_key = alloc_key(scope, "done")?;
      let value_key = alloc_key(scope, "value")?;

      let done_val = vm.get(scope, result_obj, done_key)?;
      let done = matches!(done_val, Value::Bool(true));

      let value_val = vm.get(scope, result_obj, value_key)?;
      let bytes = match value_val {
        Value::Undefined => None,
        Value::Object(obj) => {
          if !scope.heap().is_uint8_array_object(obj) {
            return Err(VmError::TypeError("expected Uint8Array value"));
          }
          Some(scope.heap().uint8_array_data(obj)?.to_vec())
        }
        _ => return Err(VmError::TypeError("expected Uint8Array value")),
      };

      let state = host
        .as_any_mut()
        .downcast_mut::<CaptureHostState>()
        .ok_or(VmError::InvariantViolation("unexpected host state type"))?;
      state.reads.push((done, bytes));
      Ok(Value::Undefined)
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);

    let _streams_guard = StreamsTeardownGuard::install(&mut vm, &realm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let env_id = bindings.env_id();

    let mut host_state = CaptureHostState::default();
    let mut hooks = JobQueueHooks::default();

    // Create a Request body stream and then drop the Request wrapper while keeping the stream
    // rooted.
    let body_obj = {
      let mut scope = heap.scope();
      let global = realm.global_object();
      let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
        return Err(VmError::InvariantViolation("Request constructor missing"));
      };

      let url_s = scope.alloc_string("https://example.com/")?;
      scope.push_root(Value::String(url_s))?;

      let init_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(init_obj))?;
      let method_s = scope.alloc_string("POST")?;
      scope.push_root(Value::String(method_s))?;
      let body_s = scope.alloc_string("hello")?;
      scope.push_root(Value::String(body_s))?;
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

      let body_key = alloc_key(&mut scope, "body")?;
      let Value::Object(body_obj) = vm.get(&mut scope, req_obj, body_key)? else {
        return Err(VmError::InvariantViolation("Request.body must return an object"));
      };

      body_obj
    };

    let body_root = heap.add_root(Value::Object(body_obj))?;

    heap.collect_garbage();
    sweep_env_state_if_gc_ran(env_id, &heap)?;

    let reader_obj = {
      let mut scope = heap.scope();
      let get_reader_key = alloc_key(&mut scope, "getReader")?;
      let get_reader_fn = vm.get(&mut scope, body_obj, get_reader_key)?;
      let Value::Object(reader_obj) = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        get_reader_fn,
        Value::Object(body_obj),
        &[],
      )?
      else {
        return Err(VmError::InvariantViolation("ReadableStream.getReader must return an object"));
      };
      reader_obj
    };
    let reader_root = heap.add_root(Value::Object(reader_obj))?;

    let capture_id = vm.register_native_call(capture_read_result_native)?;
    let func_proto = realm.intrinsics().function_prototype();

    // reader.read() -> { done:false, value: Uint8Array([104, 101, 108, 108, 111]) }
    {
      let mut scope = heap.scope();
      let read_key = alloc_key(&mut scope, "read")?;
      let read_fn = vm.get(&mut scope, reader_obj, read_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        read_fn,
        Value::Object(reader_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("read() must return a Promise object"));
      };

      let on_fulfilled = {
        let name = scope.alloc_string("onFulfilled")?;
        scope.push_root(Value::String(name))?;
        let f = scope.alloc_native_function(capture_id, None, name, 1)?;
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
        &[Value::Object(on_fulfilled), Value::Undefined],
      )?;

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
    }

    // Second read: done:true, value: undefined.
    {
      let mut scope = heap.scope();
      let read_key = alloc_key(&mut scope, "read")?;
      let read_fn = vm.get(&mut scope, reader_obj, read_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        read_fn,
        Value::Object(reader_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("read() must return a Promise object"));
      };

      let on_fulfilled = {
        let name = scope.alloc_string("onFulfilled2")?;
        scope.push_root(Value::String(name))?;
        let f = scope.alloc_native_function(capture_id, None, name, 1)?;
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
        &[Value::Object(on_fulfilled), Value::Undefined],
      )?;

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
    }

    assert_eq!(host_state.reads.len(), 2);
    assert_eq!(host_state.reads[0].0, false);
    assert_eq!(
      host_state.reads[0].1.as_deref(),
      Some(&[104u8, 101u8, 108u8, 108u8, 111u8][..])
    );
    assert_eq!(host_state.reads[1].0, true);
    assert_eq!(host_state.reads[1].1, None);

    heap.remove_root(body_root);
    heap.remove_root(reader_root);
    drop(bindings);
    drop(hooks);
    drop(host_state);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn gc_sweeps_unreferenced_requests_with_body_streams() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.invalid/")
        .with_heap_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024)),
    )?;

    let env = WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None);
    let bindings = {
      let (vm, vm_realm, heap) = realm.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<DummyHost>(vm, vm_realm, heap, env)?
    };
    let env_id = bindings.env_id();

    let baseline = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let state = lock
        .get(&env_id)
        .ok_or(VmError::InvariantViolation("fetch env state missing"))?;
      (state.requests.len(), state.request_body_stream_wrappers.len())
    };

    realm.exec_script(
      "for (let i = 0; i < 50; i++) { new Request('https://example.invalid/', { method: 'POST', body: 'x' }).body; }",
    )?;

    let before_gc = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let state = lock
        .get(&env_id)
        .ok_or(VmError::InvariantViolation("fetch env state missing"))?;
      (state.requests.len(), state.request_body_stream_wrappers.len())
    };
    assert!(
      before_gc.0 >= baseline.0 + 50,
      "expected requests to grow before GC; baseline={:?} before_gc={before_gc:?}",
      baseline
    );
    assert!(
      before_gc.1 >= baseline.1 + 50,
      "expected request body streams to grow before GC; baseline={:?} before_gc={before_gc:?}",
      baseline
    );

    realm.heap_mut().collect_garbage();
    sweep_env_state_if_gc_ran(env_id, realm.heap())?;

    let after_gc = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      let state = lock
        .get(&env_id)
        .ok_or(VmError::InvariantViolation("fetch env state missing"))?;
      (state.requests.len(), state.request_body_stream_wrappers.len())
    };
    assert_eq!(
      after_gc, baseline,
      "expected request/stream state to be swept after GC; baseline={baseline:?} after_gc={after_gc:?}"
    );

    drop(bindings);
    realm.teardown();
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
      return Err(VmError::InvariantViolation(
        "Response.error must return an object",
      ));
    };

    assert!(matches!(
      get_data_prop(&mut scope, resp_obj, "status")?,
      Value::Number(n) if n == 0.0
    ));
    assert!(matches!(
      get_data_prop(&mut scope, resp_obj, "ok")?,
      Value::Bool(false)
    ));

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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
        return Err(VmError::InvariantViolation(
          "Response.redirect must return an object",
        ));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
      return Err(VmError::InvariantViolation(
        "Response.json must return an object",
      ));
    };

    let (env_id, response_id) = response_info_from_this(&mut scope, Value::Object(resp_obj))?;
    with_env_state(env_id, scope.heap(), |state| {
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    set_data_prop(
      &mut scope,
      init_obj,
      "headers",
      Value::Object(headers_obj),
      true,
    )?;

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
      return Err(VmError::InvariantViolation(
        "Response.json must return an object",
      ));
    };

    let (env_id, response_id) = response_info_from_this(&mut scope, Value::Object(resp_obj))?;
    with_env_state(env_id, scope.heap(), |state| {
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let one = scope.alloc_bigint_from_u128(1)?;
    scope.push_root(Value::BigInt(one))?;
    let err = response_json_static_native(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      json_fn,
      Value::Object(response_ctor),
      &[Value::BigInt(one)],
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
  fn response_json_rejects_with_syntax_error_on_null_body() -> Result<(), VmError> {
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

    let mut host_state = CaptureHostState::default();
    let mut hooks = JobQueueHooks::default();

    let resp_obj = {
      let mut scope = heap.scope();
      let global = realm.global_object();
      let Value::Object(response_ctor) = get_data_prop(&mut scope, global, "Response")? else {
        return Err(VmError::InvariantViolation("Response constructor missing"));
      };

      // new Response(null, { status: 204 })
      let init_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(init_obj))?;
      set_data_prop(
        &mut scope,
        init_obj,
        "status",
        Value::Number(204.0),
        /* writable */ true,
      )?;

      let Value::Object(resp_obj) = response_ctor_construct(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        response_ctor,
        &[Value::Null, Value::Object(init_obj)],
        Value::Object(response_ctor),
      )?
      else {
        return Err(VmError::InvariantViolation(
          "Response constructor must return an object",
        ));
      };

      let body_key = alloc_key(&mut scope, "body")?;
      let body = vm.get(&mut scope, resp_obj, body_key)?;
      assert!(matches!(
        body,
        Value::Null
      ), "Response.body must be null for a null body status");

      resp_obj
    };

    let resp_root = heap.add_root(Value::Object(resp_obj))?;

    host_state.fulfilled = None;
    host_state.rejected = None;

    {
      let mut scope = heap.scope();
      let json_key = alloc_key(&mut scope, "json")?;
      let json_fn = vm.get(&mut scope, resp_obj, json_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        json_fn,
        Value::Object(resp_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("Response.json must return a Promise object"));
      };

      let capture_id = vm.register_native_call(capture_promise_error_native)?;
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

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      let on_rejected_root = scope.heap_mut().add_root(Value::Object(on_rejected))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
      heap.remove_root(on_rejected_root);
    }

    assert!(
      host_state.fulfilled.is_none(),
      "Response.json should not fulfill for a null body"
    );
    let rejected = host_state.rejected.as_deref().unwrap_or("");
    assert!(
      rejected.starts_with("SyntaxError:"),
      "expected SyntaxError rejection, got {rejected:?}"
    );

    heap.remove_root(resp_root);
    drop(bindings);
    Ok(())
  }

  #[test]
  fn request_json_rejects_with_syntax_error_on_null_body() -> Result<(), VmError> {
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

    let mut host_state = CaptureHostState::default();
    let mut hooks = JobQueueHooks::default();

    let req_obj = {
      let mut scope = heap.scope();
      let global = realm.global_object();
      let Value::Object(request_ctor) = get_data_prop(&mut scope, global, "Request")? else {
        return Err(VmError::InvariantViolation("Request constructor missing"));
      };

      let url_s = scope.alloc_string("https://example.com/")?;
      scope.push_root(Value::String(url_s))?;

      let Value::Object(req_obj) = request_ctor_construct(
        &mut vm,
        &mut scope,
        &mut host_state,
        &mut hooks,
        request_ctor,
        &[Value::String(url_s)],
        Value::Object(request_ctor),
      )?
      else {
        return Err(VmError::InvariantViolation(
          "Request constructor must return an object",
        ));
      };

      let body_key = alloc_key(&mut scope, "body")?;
      let body = vm.get(&mut scope, req_obj, body_key)?;
      assert!(
        matches!(body, Value::Null),
        "Request.body must be null when no body was provided"
      );

      req_obj
    };

    let req_root = heap.add_root(Value::Object(req_obj))?;

    host_state.fulfilled = None;
    host_state.rejected = None;

    {
      let mut scope = heap.scope();
      let json_key = alloc_key(&mut scope, "json")?;
      let json_fn = vm.get(&mut scope, req_obj, json_key)?;
      let promise = vm.call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        json_fn,
        Value::Object(req_obj),
        &[],
      )?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::InvariantViolation("Request.json must return a Promise object"));
      };

      let capture_id = vm.register_native_call(capture_promise_error_native)?;
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

      let promise_root = scope.heap_mut().add_root(Value::Object(promise_obj))?;
      let on_fulfilled_root = scope.heap_mut().add_root(Value::Object(on_fulfilled))?;
      let on_rejected_root = scope.heap_mut().add_root(Value::Object(on_rejected))?;
      drop(scope);
      drain_jobs(&mut vm, &mut heap, &mut host_state, &mut hooks)?;
      heap.remove_root(promise_root);
      heap.remove_root(on_fulfilled_root);
      heap.remove_root(on_rejected_root);
    }

    assert!(
      host_state.fulfilled.is_none(),
      "Request.json should not fulfill for a null body"
    );
    let rejected = host_state.rejected.as_deref().unwrap_or("");
    assert!(
      rejected.starts_with("SyntaxError:"),
      "expected SyntaxError rejection, got {rejected:?}"
    );

    heap.remove_root(req_root);
    drop(bindings);
    Ok(())
  }

  #[test]
  fn fetch_rejects_non_object_init() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    set_data_prop(
      &mut scope,
      init_obj,
      "Set-Cookie",
      Value::String(a_val),
      true,
    )?;
    set_data_prop(&mut scope, init_obj, "Other", Value::String(x_val), true)?;
    set_data_prop(
      &mut scope,
      init_obj,
      "set-cookie",
      Value::String(b_val),
      true,
    )?;

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
      return Err(VmError::InvariantViolation(
        "Headers constructor must return an object",
      ));
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
      return Err(VmError::InvariantViolation(
        "Headers.getSetCookie must return an object",
      ));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
  fn request_clone_rejects_locked_body_stream() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let _streams_guard = StreamsTeardownGuard::install(&mut vm, &realm, &mut heap)?;
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

    // new Request(url, { method: "POST", body: "hello" })
    let init_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(init_obj))?;
    let body_s = scope.alloc_string("hello")?;
    scope.push_root(Value::String(body_s))?;
    let method_s = scope.alloc_string("POST")?;
    scope.push_root(Value::String(method_s))?;
    set_data_prop(
      &mut scope,
      init_obj,
      "method",
      Value::String(method_s),
      true,
    )?;
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
      return Err(VmError::InvariantViolation(
        "Request constructor must return an object",
      ));
    };
    scope.push_root(Value::Object(req_obj))?;

    // Lock the body stream via `req.body.getReader()`.
    let body_key = alloc_key(&mut scope, "body")?;
    let body_val = vm.get(&mut scope, req_obj, body_key)?;
    let Value::Object(body_stream_obj) = body_val else {
      return Err(VmError::InvariantViolation(
        "Request.body must return an object when the request has a body",
      ));
    };
    scope.push_root(Value::Object(body_stream_obj))?;

    let get_reader_key = alloc_key(&mut scope, "getReader")?;
    let get_reader = vm.get(&mut scope, body_stream_obj, get_reader_key)?;
    vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      get_reader,
      Value::Object(body_stream_obj),
      &[],
    )?;

    // Once the underlying ReadableStream is locked, Request.prototype.clone should throw.
    let clone_key = alloc_key(&mut scope, "clone")?;
    let clone_fn = vm.get(&mut scope, req_obj, clone_key)?;
    let err = vm
      .call_with_host_and_hooks(
        &mut host_state,
        &mut scope,
        &mut hooks,
        clone_fn,
        Value::Object(req_obj),
        &[],
      )
      .expect_err("expected TypeError for locked body stream");
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
    assert_eq!(
      scope.heap().get_string(name_s)?.to_utf8_lossy(),
      "TypeError"
    );
    assert_eq!(
      scope.heap().get_string(msg_s)?.to_utf8_lossy(),
      "Request body is locked"
    );

    drop(scope);
    drop(bindings);
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    set_data_prop(
      &mut scope,
      init_obj,
      "method",
      Value::String(method_s),
      true,
    )?;
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
      return Err(VmError::InvariantViolation(
        "Request constructor must return an object",
      ));
    };

    // Mark the body as used directly in the backing state.
    let request_id = number_to_u64(get_data_prop(&mut scope, req_obj, REQUEST_ID_KEY)?)?;
    with_env_state_mut(env_id, scope.heap(), |state| {
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
    assert_eq!(
      scope.heap().get_string(name_s)?.to_utf8_lossy(),
      "TypeError"
    );
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    set_data_prop(
      &mut scope,
      init_obj,
      "method",
      Value::String(method_s),
      true,
    )?;
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
      return Err(VmError::InvariantViolation(
        "Request constructor must return an object",
      ));
    };

    let request_id = number_to_u64(get_data_prop(&mut scope, req_obj, REQUEST_ID_KEY)?)?;
    with_env_state_mut(env_id, scope.heap(), |state| {
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
    assert_eq!(
      scope.heap().get_string(name_s)?.to_utf8_lossy(),
      "TypeError"
    );
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
  fn request_ctor_and_fetch_reject_locked_body_input_request() -> Result<(), VmError> {
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let _realm_guard = RealmTeardownGuard::new(&mut realm, &mut heap);
    let _streams_guard = StreamsTeardownGuard::install(&mut vm, &realm, &mut heap)?;
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
    set_data_prop(
      &mut scope,
      init_obj,
      "method",
      Value::String(method_s),
      true,
    )?;
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
      return Err(VmError::InvariantViolation(
        "Request constructor must return an object",
      ));
    };
    scope.push_root(Value::Object(req_obj))?;

    // Lock the real body stream via `req.body.getReader()`.
    let body_key = alloc_key(&mut scope, "body")?;
    let Value::Object(body_stream_obj) = vm.get(&mut scope, req_obj, body_key)? else {
      return Err(VmError::InvariantViolation("Request.body must be an object"));
    };
    let get_reader_key = alloc_key(&mut scope, "getReader")?;
    let get_reader = vm.get(&mut scope, body_stream_obj, get_reader_key)?;
    let Value::Object(reader_obj) = vm.call_with_host_and_hooks(
      &mut host_state,
      &mut scope,
      &mut hooks,
      get_reader,
      Value::Object(body_stream_obj),
      &[],
    )?
    else {
      return Err(VmError::InvariantViolation(
        "ReadableStream.getReader must return an object",
      ));
    };
    scope.push_root(Value::Object(reader_obj))?;

    // new Request(existingRequest) should throw if the input body stream is locked and init.body is
    // not specified.
    let err = request_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      request_ctor,
      &[Value::Object(req_obj)],
      Value::Object(request_ctor),
    )
    .expect_err("expected TypeError for locked body");

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
    assert_eq!(
      scope.heap().get_string(name_s)?.to_utf8_lossy(),
      "TypeError"
    );
    let msg = scope.heap().get_string(msg_s)?.to_utf8_lossy();
    assert!(msg.contains("body") && msg.contains("locked"), "msg={msg:?}");

    // fetch(existingRequest) should also throw/reject with TypeError in the locked-body case.
    let err = fetch_call::<DummyHost>(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      fetch_fn,
      Value::Undefined,
      &[Value::Object(req_obj)],
    )
    .expect_err("expected TypeError for locked body");
    let Some(Value::Object(err_obj)) = err.thrown_value() else {
      panic!("expected thrown TypeError object, got {err:?}");
    };
    scope.push_root(Value::Object(err_obj))?;
    let msg_key = alloc_key(&mut scope, "message")?;
    let Value::String(msg_s) = vm.get(&mut scope, err_obj, msg_key)? else {
      return Err(VmError::InvariantViolation("TypeError.message missing"));
    };
    let msg = scope.heap().get_string(msg_s)?.to_utf8_lossy();
    assert!(msg.contains("body") && msg.contains("locked"), "msg={msg:?}");

    // Overriding init.body should skip the locked-input-body check.
    let override_init = scope.alloc_object()?;
    scope.push_root(Value::Object(override_init))?;
    let override_body = scope.alloc_string("override")?;
    scope.push_root(Value::String(override_body))?;
    let override_method = scope.alloc_string("POST")?;
    scope.push_root(Value::String(override_method))?;
    set_data_prop(
      &mut scope,
      override_init,
      "method",
      Value::String(override_method),
      true,
    )?;
    set_data_prop(
      &mut scope,
      override_init,
      "body",
      Value::String(override_body),
      true,
    )?;

    let Value::Object(_override_req) = request_ctor_construct(
      &mut vm,
      &mut scope,
      &mut host_state,
      &mut hooks,
      request_ctor,
      &[Value::Object(req_obj), Value::Object(override_init)],
      Value::Object(request_ctor),
    )?
    else {
      return Err(VmError::InvariantViolation(
        "Request constructor must return an object",
      ));
    };

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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
    set_data_prop(
      &mut scope,
      headers_init,
      "x-test",
      Value::String(x_value),
      true,
    )?;
    set_data_prop(
      &mut scope,
      init_obj,
      "headers",
      Value::Object(headers_init),
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
      return Err(VmError::InvariantViolation(
        "Request constructor must return an object",
      ));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
      return Err(VmError::InvariantViolation(
        "Request constructor must return an object",
      ));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
      set_data_prop(
        &mut scope,
        init_obj,
        "method",
        Value::String(method_s),
        true,
      )?;
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
        return Err(VmError::InvariantViolation(
          "Request constructor must return an object",
        ));
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
        return Err(VmError::InvariantViolation(
          "Request.clone must return an object",
        ));
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
        return Err(VmError::InvariantViolation(
          "Request.text must return a Promise object",
        ));
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
        return Err(VmError::InvariantViolation(
          "Request.text must return a Promise object",
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

      // Smoke test for `json()` consumption.
      let mut scope = heap.scope();
      let url_s = scope.alloc_string("https://example.com/")?;
      scope.push_root(Value::String(url_s))?;
      let init_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(init_obj))?;
      let json_body = scope.alloc_string("null")?;
      let json_method = scope.alloc_string("POST")?;
      scope.push_root(Value::String(json_method))?;
      set_data_prop(
        &mut scope,
        init_obj,
        "method",
        Value::String(json_method),
        true,
      )?;
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
        return Err(VmError::InvariantViolation(
          "Request constructor must return an object",
        ));
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
        return Err(VmError::InvariantViolation(
          "Request.json must return a Promise object",
        ));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
      set_data_prop(
        &mut scope,
        init_obj,
        "method",
        Value::String(method_s),
        true,
      )?;
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
        return Err(VmError::InvariantViolation(
          "Request constructor must return an object",
        ));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
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
      return Err(VmError::InvariantViolation(
        "Headers constructor must return an object",
      ));
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
      return Err(VmError::InvariantViolation(
        "Headers.entries must return an object",
      ));
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
      return Err(VmError::InvariantViolation(
        "Iterator result must be an object",
      ));
    };
    let done_key = alloc_key(&mut scope, "done")?;
    let value_key = alloc_key(&mut scope, "value")?;
    assert!(matches!(
      vm.get(&mut scope, res1_obj, done_key)?,
      Value::Bool(false)
    ));
    let Value::Object(pair1) = vm.get(&mut scope, res1_obj, value_key)? else {
      return Err(VmError::InvariantViolation(
        "Iterator value must be an object",
      ));
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
      return Err(VmError::InvariantViolation(
        "Iterator result must be an object",
      ));
    };
    assert!(matches!(
      vm.get(&mut scope, res2_obj, done_key)?,
      Value::Bool(false)
    ));
    let Value::Object(pair2) = vm.get(&mut scope, res2_obj, value_key)? else {
      return Err(VmError::InvariantViolation(
        "Iterator value must be an object",
      ));
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
    let mut heap = Heap::new(HeapLimits::new(TEST_HEAP_BYTES, TEST_HEAP_BYTES));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let bindings = install_window_fetch_bindings_with_guard::<DummyHost>(
      &mut vm,
      &realm,
      &mut heap,
      WindowFetchEnv::for_document(Arc::new(crate::resource::HttpFetcher::new()), None),
    )?;
    let env_id = bindings.env_id();

    let response_id = with_env_state_mut(env_id, &heap, |state| {
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
    let Value::Object(response_proto) = get_data_prop(&mut scope, response_ctor, "prototype")?
    else {
      return Err(VmError::InvariantViolation("Response.prototype missing"));
    };

    let resp_obj = make_response_wrapper(
      &mut scope,
      env_id,
      headers_proto,
      response_proto,
      response_id,
    )?;
    let Value::String(type_s) = get_data_prop(&mut scope, resp_obj, "type")? else {
      return Err(VmError::InvariantViolation("Response.type missing"));
    };
    let ty = scope.heap().get_string(type_s)?.to_utf8_lossy();
    assert_eq!(ty, "cors");
    assert!(matches!(
      get_data_prop(&mut scope, resp_obj, "redirected")?,
      Value::Bool(true)
    ));

    drop(scope);
    drop(bindings);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn fetch_env_gc_sweeps_wrapper_backing_state() -> Result<(), VmError> {
    let mut opts = JsExecutionOptions::default();
    // This test allocates many wrapper objects; allow extra time so it doesn't trip the default
    // per-run wall-time limit.
    opts.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));
    let mut host = EventLoopHost::new_with_js_execution_options(opts);

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };
    let env_id = bindings.env_id();

    let baseline = with_env_state(env_id, host.window.heap(), |state| {
      Ok((
        state.requests.len(),
        state.responses.len(),
        state.owned_headers.len(),
        state.headers_iterators.len(),
      ))
    })?;

    // Allocate many temporary wrapper objects so the per-env Rust registries grow.
    host.window.exec_script(
      "(function(){\
          for (let i = 0; i < 250; i++) new Request('https://example.invalid/' + i);\
          for (let j = 0; j < 250; j++) new Response('hi');\
          for (let k = 0; k < 250; k++) new Headers({ a: 'b' });\
          const h = new Headers({ a: 'b' });\
          for (let l = 0; l < 250; l++) h.entries();\
        })()",
    )?;

    let grown = with_env_state(env_id, host.window.heap(), |state| {
      Ok((
        state.requests.len(),
        state.responses.len(),
        state.owned_headers.len(),
        state.headers_iterators.len(),
      ))
    })?;

    // Force a GC cycle, then trigger the opportunistic sweep path.
    host.window.heap_mut().collect_garbage();
    sweep_env_state_if_gc_ran(env_id, host.window.heap())?;

    let swept = with_env_state(env_id, host.window.heap(), |state| {
      Ok((
        state.requests.len(),
        state.responses.len(),
        state.owned_headers.len(),
        state.headers_iterators.len(),
      ))
    })?;
    assert!(
      swept.0 <= baseline.0 + 2,
      "requests not swept: grown={grown:?} swept={swept:?} baseline={baseline:?}"
    );
    assert!(
      swept.1 <= baseline.1 + 2,
      "responses not swept: grown={grown:?} swept={swept:?} baseline={baseline:?}"
    );
    assert!(
      swept.2 <= baseline.2 + 2,
      "owned_headers not swept: grown={grown:?} swept={swept:?} baseline={baseline:?}"
    );
    assert!(
      swept.3 <= baseline.3 + 2,
      "headers_iterators not swept: grown={grown:?} swept={swept:?} baseline={baseline:?}"
    );

    drop(bindings);
    Ok(())
  }

  #[test]
  fn headers_wrapper_keeps_response_alive_across_gc_sweep() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };
    let env_id = bindings.env_id();

    let baseline_responses = with_env_state(env_id, host.window.heap(), |state| Ok(state.responses.len()))?;

    host.window.exec_script(
      "(function(){\
          let r = new Response('hi', { headers: { 'X-Test': '1' } });\
          globalThis.__h = r.headers;\
          r = null;\
        })()",
    )?;

    host.window.heap_mut().collect_garbage();
    sweep_env_state_if_gc_ran(env_id, host.window.heap())?;

    // The Headers wrapper should still work after GC even though the owning Response variable was
    // cleared.
    let value = host.window.exec_script("globalThis.__h.get('x-test')")?;
    let Value::String(value_s) = value else {
      return Err(VmError::InvariantViolation(
        "Headers.get should return a string for an existing header",
      ));
    };
    assert_eq!(host.window.heap().get_string(value_s)?.to_utf8_lossy(), "1");

    // The response backing state should not have been swept while Headers is alive.
    let responses_after_gc = with_env_state(env_id, host.window.heap(), |state| Ok(state.responses.len()))?;
    assert_eq!(responses_after_gc, baseline_responses + 1);

    // Once the Headers wrapper is dropped, the Response should be eligible for sweeping again.
    host.window.exec_script("globalThis.__h = null")?;
    host.window.heap_mut().collect_garbage();
    sweep_env_state_if_gc_ran(env_id, host.window.heap())?;

    let responses_after_drop = with_env_state(env_id, host.window.heap(), |state| Ok(state.responses.len()))?;
    assert_eq!(responses_after_drop, baseline_responses);

    drop(bindings);
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
    assert_eq!(
      scope.heap().get_string(type_s)?.to_utf8_lossy(),
      "text/plain"
    );

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
    assert_eq!(
      scope.heap().get_string(type_s)?.to_utf8_lossy(),
      "text/plain"
    );

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
  fn response_ctor_accepts_data_view_body() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host.window.exec_script(
      "(function(){\
         const buf = new ArrayBuffer(4);\
         const u8 = new Uint8Array(buf);\
         u8[0] = 1;\
         u8[1] = 2;\
         u8[2] = 3;\
         u8[3] = 4;\
         const view = new DataView(buf, 1, 2);\
         return new Response(view).arrayBuffer();\
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
    assert_eq!(scope.heap().array_buffer_data(ab_obj)?, &[2, 3]);

    Ok(())
  }

  #[test]
  fn response_bytes_resolves_to_uint8_array_for_uint8_array_body() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host
      .window
      .exec_script("new Response(new Uint8Array([1,2,3])).bytes()")?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Response.bytes must return a Promise object",
      ));
    };
    assert_eq!(
      host.window.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = host.window.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Response.bytes promise missing result",
      ));
    };
    let Value::Object(u8_obj) = result else {
      return Err(VmError::InvariantViolation(
        "Response.bytes must resolve to a Uint8Array object",
      ));
    };

    let (_vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(u8_obj))?;
    assert!(scope.heap().is_uint8_array_object(u8_obj));
    assert_eq!(scope.heap().uint8_array_data(u8_obj)?, &[1, 2, 3]);

    Ok(())
  }

  #[test]
  fn request_bytes_resolves_to_uint8_array_for_uint8_array_body() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host.window.exec_script(
      "new Request('https://x/', {method:'POST', body:new Uint8Array([4,5])}).bytes()",
    )?;
    let Value::Object(promise_obj) = promise else {
      return Err(VmError::InvariantViolation(
        "Request.bytes must return a Promise object",
      ));
    };
    assert_eq!(
      host.window.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = host.window.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Request.bytes promise missing result",
      ));
    };
    let Value::Object(u8_obj) = result else {
      return Err(VmError::InvariantViolation(
        "Request.bytes must resolve to a Uint8Array object",
      ));
    };

    let (_vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(u8_obj))?;
    assert!(scope.heap().is_uint8_array_object(u8_obj));
    assert_eq!(scope.heap().uint8_array_data(u8_obj)?, &[4, 5]);

    Ok(())
  }

  #[test]
  fn response_bytes_accepts_js_created_byte_stream_body() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host.window.exec_script(
      "(function(){\
         const stream = new ReadableStream({\
           start(c) {\
             c.enqueue(new Uint8Array([7, 8]));\
             c.close();\
           }\
         });\
         return new Response(stream).bytes();\
       })()",
    )?;
    let promise_root = host.window.heap_mut().add_root(promise)?;

    // Stream-backed body consumers resolve asynchronously via Promise jobs.
    host.window.perform_microtask_checkpoint()?;

    let promise_val = host
      .window
      .heap()
      .get_root(promise_root)
      .unwrap_or(Value::Undefined);
    let Value::Object(promise_obj) = promise_val else {
      return Err(VmError::InvariantViolation(
        "Response.bytes must return a Promise object",
      ));
    };
    assert_eq!(
      host.window.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = host.window.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Response.bytes promise missing result",
      ));
    };
    let Value::Object(u8_obj) = result else {
      return Err(VmError::InvariantViolation(
        "Response.bytes must resolve to a Uint8Array object",
      ));
    };

    {
      let (_vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(u8_obj))?;
      assert!(scope.heap().is_uint8_array_object(u8_obj));
      assert_eq!(scope.heap().uint8_array_data(u8_obj)?, &[7, 8]);
    }

    host.window.heap_mut().remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn bytes_sets_body_used_and_rejects_on_second_read() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result_val = host.window.exec_script(
      "(function(){\
         const resp = new Response(new Uint8Array([1,2,3]));\
         const usedBefore = resp.bodyUsed;\
         const p1 = resp.bytes();\
         const usedAfter = resp.bodyUsed;\
         const p2 = resp.bytes();\
         return { usedBefore, usedAfter, p1, p2 };\
       })()",
    )?;

    let Value::Object(result_obj) = result_val else {
      return Err(VmError::InvariantViolation(
        "expected bytes bodyUsed test script to return an object",
      ));
    };

    let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(result_obj))?;

    let used_before_key = alloc_key(&mut scope, "usedBefore")?;
    assert_eq!(vm.get(&mut scope, result_obj, used_before_key)?, Value::Bool(false));

    let used_after_key = alloc_key(&mut scope, "usedAfter")?;
    assert_eq!(vm.get(&mut scope, result_obj, used_after_key)?, Value::Bool(true));

    let p1_key = alloc_key(&mut scope, "p1")?;
    let p1_val = vm.get(&mut scope, result_obj, p1_key)?;
    let Value::Object(p1_obj) = p1_val else {
      return Err(VmError::InvariantViolation(
        "expected bytes bodyUsed test script to return p1 as a Promise object",
      ));
    };
    assert_eq!(scope.heap().promise_state(p1_obj)?, PromiseState::Fulfilled);
    let Some(p1_result) = scope.heap().promise_result(p1_obj)? else {
      return Err(VmError::InvariantViolation("bytes p1 promise missing result"));
    };
    let Value::Object(p1_u8_obj) = p1_result else {
      return Err(VmError::InvariantViolation(
        "bytes p1 promise must resolve to a Uint8Array object",
      ));
    };
    scope.push_root(Value::Object(p1_u8_obj))?;
    assert!(scope.heap().is_uint8_array_object(p1_u8_obj));
    assert_eq!(scope.heap().uint8_array_data(p1_u8_obj)?, &[1, 2, 3]);

    let p2_key = alloc_key(&mut scope, "p2")?;
    let p2_val = vm.get(&mut scope, result_obj, p2_key)?;
    let Value::Object(p2_obj) = p2_val else {
      return Err(VmError::InvariantViolation(
        "expected bytes bodyUsed test script to return p2 as a Promise object",
      ));
    };
    assert_eq!(scope.heap().promise_state(p2_obj)?, PromiseState::Rejected);
    let Some(reason) = scope.heap().promise_result(p2_obj)? else {
      return Err(VmError::InvariantViolation("bytes p2 rejected promise missing reason"));
    };
    let Value::Object(err_obj) = reason else {
      return Err(VmError::InvariantViolation(
        "bytes p2 rejected promise reason must be an object",
      ));
    };
    scope.push_root(Value::Object(err_obj))?;
    let message_key = alloc_key(&mut scope, "message")?;
    let message_val = vm.get(&mut scope, err_obj, message_key)?;
    let msg = get_string(scope.heap(), message_val);
    assert!(
      msg.contains("body is already used"),
      "expected bytes second call rejection to mention BodyUsed, got {msg:?}"
    );

    Ok(())
  }

  #[test]
  fn response_array_buffer_accepts_js_created_byte_stream_body() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let promise = host.window.exec_script(
      "(function(){\
         const stream = new ReadableStream({\
           start(c) {\
             c.enqueue(new Uint8Array([1, 2, 3]));\
             c.close();\
           }\
         });\
         return new Response(stream).arrayBuffer();\
       })()",
    )?;
    let promise_root = host.window.heap_mut().add_root(promise)?;

    // Stream-backed body consumers resolve asynchronously via Promise jobs.
    host.window.perform_microtask_checkpoint()?;

    let promise_val = host
      .window
      .heap()
      .get_root(promise_root)
      .unwrap_or(Value::Undefined);
    let Value::Object(promise_obj) = promise_val else {
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

    {
      let (_vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(ab_obj))?;
      assert!(scope.heap().is_array_buffer_object(ab_obj));
      assert_eq!(scope.heap().array_buffer_data(ab_obj)?, &[1, 2, 3]);
    }

    host.window.heap_mut().remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn response_ctor_accepts_readable_stream_body_and_text_consumes_it() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result_value = host.window.exec_script(
      "(function(){\
         const upstream = new Response('hi');\
         const stream = upstream.body;\
         const resp = new Response(stream);\
         return { same: resp.body === stream, promise: resp.text() };\
       })()",
    )?;
    let result_root = host.window.heap_mut().add_root(result_value)?;

    // Stream-backed body consumers resolve asynchronously via Promise jobs.
    host.window.perform_microtask_checkpoint()?;

    let result_value = host
      .window
      .heap()
      .get_root(result_root)
      .unwrap_or(Value::Undefined);
    let Value::Object(result_obj) = result_value else {
      return Err(VmError::InvariantViolation(
        "expected response stream body test script to return an object",
      ));
    };

    {
      let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(result_obj))?;

      let same_key = alloc_key(&mut scope, "same")?;
      assert_eq!(vm.get(&mut scope, result_obj, same_key)?, Value::Bool(true));

      let promise_key = alloc_key(&mut scope, "promise")?;
      let promise_val = vm.get(&mut scope, result_obj, promise_key)?;
      let Value::Object(promise_obj) = promise_val else {
        return Err(VmError::InvariantViolation(
          "expected response stream body test script to return a promise object",
        ));
      };
      assert_eq!(
        scope.heap().promise_state(promise_obj)?,
        PromiseState::Fulfilled
      );
      let Some(result) = scope.heap().promise_result(promise_obj)? else {
        return Err(VmError::InvariantViolation(
          "Response.text promise missing result",
        ));
      };
      let Value::String(text_s) = result else {
        return Err(VmError::InvariantViolation(
          "Response.text must resolve to a string",
        ));
      };
      assert_eq!(scope.heap().get_string(text_s)?.to_utf8_lossy(), "hi");
    }

    host.window.heap_mut().remove_root(result_root);
    Ok(())
  }

  #[test]
  fn response_ctor_accepts_user_readable_stream_bodyinit_and_text_consumes_it() -> Result<(), VmError>
  {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result_value = host.window.exec_script(
      "(function(){\
         const s = new ReadableStream({ start(c){ c.enqueue('hi'); c.close(); } });\
         const bytes = s.pipeThrough(new TextEncoderStream());\
         const resp = new Response(bytes);\
         return { same: resp.body === bytes, usedBefore: resp.bodyUsed, resp, promise: resp.text() };\
       })()",
    )?;
    let result_root = host.window.heap_mut().add_root(result_value)?;

    // Stream-backed body consumers resolve asynchronously via Promise jobs.
    host.window.perform_microtask_checkpoint()?;

    let result_value = host
      .window
      .heap()
      .get_root(result_root)
      .unwrap_or(Value::Undefined);
    let Value::Object(result_obj) = result_value else {
      return Err(VmError::InvariantViolation(
        "expected response stream bodyinit test script to return an object",
      ));
    };

    {
      let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(result_obj))?;

      let same_key = alloc_key(&mut scope, "same")?;
      assert_eq!(vm.get(&mut scope, result_obj, same_key)?, Value::Bool(true));

      let used_before_key = alloc_key(&mut scope, "usedBefore")?;
      assert_eq!(
        vm.get(&mut scope, result_obj, used_before_key)?,
        Value::Bool(false)
      );

      let promise_key = alloc_key(&mut scope, "promise")?;
      let promise_val = vm.get(&mut scope, result_obj, promise_key)?;
      let Value::Object(promise_obj) = promise_val else {
        return Err(VmError::InvariantViolation(
          "expected response stream bodyinit test script to return a promise object",
        ));
      };
      assert_eq!(
        scope.heap().promise_state(promise_obj)?,
        PromiseState::Fulfilled
      );
      let Some(result) = scope.heap().promise_result(promise_obj)? else {
        return Err(VmError::InvariantViolation(
          "Response.text promise missing result",
        ));
      };
      let Value::String(text_s) = result else {
        return Err(VmError::InvariantViolation(
          "Response.text must resolve to a string",
        ));
      };
      assert_eq!(scope.heap().get_string(text_s)?.to_utf8_lossy(), "hi");

      let resp_key = alloc_key(&mut scope, "resp")?;
      let resp_val = vm.get(&mut scope, result_obj, resp_key)?;
      let Value::Object(resp_obj) = resp_val else {
        return Err(VmError::InvariantViolation(
          "expected response stream bodyinit test script to return a Response object",
        ));
      };
      scope.push_root(Value::Object(resp_obj))?;
      let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
      assert_eq!(vm.get(&mut scope, resp_obj, body_used_key)?, Value::Bool(true));
    }

    host.window.heap_mut().remove_root(result_root);
    Ok(())
  }

  #[test]
  fn response_clone_works_for_stream_body() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result_value = host.window.exec_script(
      "(function(){\
         const upstream = new Response('hi');\
         const stream = upstream.body;\
         const resp = new Response(stream);\
         const clone = resp.clone();\
         const usedBefore = [resp.bodyUsed, clone.bodyUsed];\
         const promise = Promise.all([resp.text(), clone.text()]);\
         return { usedBefore, promise, resp, clone };\
       })()",
    )?;
    let result_root = host.window.heap_mut().add_root(result_value)?;

    // Stream-backed body consumers resolve asynchronously via Promise jobs.
    host.window.perform_microtask_checkpoint()?;

    let result_value = host
      .window
      .heap()
      .get_root(result_root)
      .unwrap_or(Value::Undefined);
    let Value::Object(result_obj) = result_value else {
      return Err(VmError::InvariantViolation(
        "expected response clone stream body test script to return an object",
      ));
    };

    {
      let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(result_obj))?;

      let used_before_key = alloc_key(&mut scope, "usedBefore")?;
      let used_before_val = vm.get(&mut scope, result_obj, used_before_key)?;
      let Value::Object(used_before_arr) = used_before_val else {
        return Err(VmError::InvariantViolation(
          "expected usedBefore to be an array-like object",
        ));
      };
      scope.push_root(Value::Object(used_before_arr))?;
      let idx0_key = alloc_key(&mut scope, "0")?;
      let idx1_key = alloc_key(&mut scope, "1")?;
      assert_eq!(
        vm.get(&mut scope, used_before_arr, idx0_key)?,
        Value::Bool(false)
      );
      assert_eq!(
        vm.get(&mut scope, used_before_arr, idx1_key)?,
        Value::Bool(false)
      );

      let promise_key = alloc_key(&mut scope, "promise")?;
      let promise_val = vm.get(&mut scope, result_obj, promise_key)?;
      let Value::Object(promise_obj) = promise_val else {
        return Err(VmError::InvariantViolation(
          "expected response clone stream body test script to return a promise object",
        ));
      };
      assert_eq!(
        scope.heap().promise_state(promise_obj)?,
        PromiseState::Fulfilled
      );
      let Some(result) = scope.heap().promise_result(promise_obj)? else {
        return Err(VmError::InvariantViolation(
          "Response.text Promise.all promise missing result",
        ));
      };
      let Value::Object(results_arr) = result else {
        return Err(VmError::InvariantViolation(
          "Response.text Promise.all must resolve to an array-like object",
        ));
      };
      scope.push_root(Value::Object(results_arr))?;
      let idx0_key = alloc_key(&mut scope, "0")?;
      let idx1_key = alloc_key(&mut scope, "1")?;
      let Value::String(t0_s) = vm.get(&mut scope, results_arr, idx0_key)? else {
        return Err(VmError::InvariantViolation(
          "Response.text Promise.all result[0] must be a string",
        ));
      };
      let Value::String(t1_s) = vm.get(&mut scope, results_arr, idx1_key)? else {
        return Err(VmError::InvariantViolation(
          "Response.text Promise.all result[1] must be a string",
        ));
      };
      let t0 = scope.heap().get_string(t0_s)?.to_utf8_lossy();
      let t1 = scope.heap().get_string(t1_s)?.to_utf8_lossy();
      assert_eq!(t0, t1);
      assert_eq!(t0, "hi");

      let resp_key = alloc_key(&mut scope, "resp")?;
      let clone_key = alloc_key(&mut scope, "clone")?;
      let Value::Object(resp_obj) = vm.get(&mut scope, result_obj, resp_key)? else {
        return Err(VmError::InvariantViolation("expected resp to be an object"));
      };
      let Value::Object(clone_obj) = vm.get(&mut scope, result_obj, clone_key)? else {
        return Err(VmError::InvariantViolation("expected clone to be an object"));
      };
      let body_used_key = alloc_key(&mut scope, "bodyUsed")?;
      assert_eq!(vm.get(&mut scope, resp_obj, body_used_key)?, Value::Bool(true));
      assert_eq!(vm.get(&mut scope, clone_obj, body_used_key)?, Value::Bool(true));
    }

    host.window.heap_mut().remove_root(result_root);
    Ok(())
  }

  #[test]
  fn response_ctor_does_not_call_to_string_on_readable_stream_bodyinit() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result = host.window.exec_script(
      "(function(){\
         const s = new ReadableStream({ start(c){ c.enqueue('hi'); c.close(); } });\
         const bytes = s.pipeThrough(new TextEncoderStream());\
         bytes.toString = () => { throw new Error('toString called'); };\
         new Response(bytes);\
         return 'ok';\
       })()",
    )?;
    assert_eq!(get_string(host.window.heap(), result), "ok");

    Ok(())
  }

  #[test]
  fn request_ctor_does_not_call_to_string_on_readable_stream_bodyinit() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result = host.window.exec_script(
      "(function(){\
         const s = new ReadableStream({ start(c){ c.enqueue('hi'); c.close(); } });\
         const bytes = s.pipeThrough(new TextEncoderStream());\
         bytes.toString = () => { throw new Error('toString called'); };\
         try {\
           new Request('https://x/', { method: 'POST', body: bytes });\
           return { ok: true };\
         } catch (e) {\
           return { ok: false, name: e.name, message: e.message };\
         }\
       })()",
    )?;

    let Value::Object(result_obj) = result else {
      return Err(VmError::InvariantViolation(
        "expected Request ReadableStream rejection test script to return an object",
      ));
    };

    let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(result_obj))?;

    let ok_key = alloc_key(&mut scope, "ok")?;
    assert_eq!(vm.get(&mut scope, result_obj, ok_key)?, Value::Bool(true));

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
      return Err(VmError::InvariantViolation(
        "expected promise to be an object",
      ));
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = scope.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Response.text promise missing result",
      ));
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
      return Err(VmError::InvariantViolation(
        "expected promise to be an object",
      ));
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = scope.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Response.text promise missing result",
      ));
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
  fn response_ctor_accepts_file_body_and_sets_content_type() -> Result<(), VmError> {
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)?
    };

    let result_obj = host.window.exec_script(
      "(function(){\
         const resp = new Response(new File(['hi'], 'x.txt', { type: 'Text/Plain' }));\
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
      return Err(VmError::InvariantViolation(
        "Response.text promise missing result",
      ));
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
      return Err(VmError::InvariantViolation(
        "expected promise to be an object",
      ));
    };
    assert_eq!(
      scope.heap().promise_state(promise_obj)?,
      PromiseState::Fulfilled
    );
    let Some(result) = scope.heap().promise_result(promise_obj)? else {
      return Err(VmError::InvariantViolation(
        "Response.text promise missing result",
      ));
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

  #[test]
  fn response_ctor_form_data_defaults_filename_from_file_name() -> Result<(), VmError> {
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
           fd.append('file', new File(['hi'], 'f.txt', { type: 'text/plain' }));
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
    })?;
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
      window_form_data::FormDataValue::File { data, filename, .. } => {
        assert_eq!(filename, "f.txt");
        assert_eq!(data.r#type, "text/plain");
        assert_eq!(data.bytes.as_slice(), b"hi");
      }
      other => panic!("expected file entry, got {other:?}"),
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
      Self {
        host_ctx: (),
        window,
      }
    }
  }

  impl WindowRealmHost for EventLoopHost {
    fn vm_host_and_window_realm(
      &mut self,
    ) -> crate::error::Result<(&mut dyn vm_js::VmHost, &mut WindowRealm)> {
      let EventLoopHost { host_ctx, window } = self;
      Ok((host_ctx, window))
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

  struct FinalUrlWithFragmentFetcher;

  impl ResourceFetcher for FinalUrlWithFragmentFetcher {
    fn fetch(&self, url: &str) -> crate::error::Result<FetchedResource> {
      Ok(FetchedResource::with_final_url(
        format!("ok:{url}").into_bytes(),
        Some("text/plain".to_string()),
        Some(format!("{url}#frag")),
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

    fn fetch_http_request(
      &self,
      req: crate::resource::HttpRequest<'_>,
    ) -> crate::error::Result<FetchedResource> {
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
  fn fetch_response_url_excludes_fragment() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());
    let env = WindowFetchEnv::for_document(
      Arc::new(FinalUrlWithFragmentFetcher),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            (async () => {
              const resp = await fetch('https://example.invalid/ok');
              globalThis.__url = resp.url;
            })();
          "#,
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let url_val = host.window.exec_script("globalThis.__url").unwrap();
    assert_eq!(get_string(host.window.heap(), url_val), "https://example.invalid/ok");
    Ok(())
  }

  #[test]
  fn response_body_stream_can_be_consumed_with_for_await() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());
    let env = WindowFetchEnv::for_document(
      Arc::new(StaticOkFetcher),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            globalThis.__out = "";
            globalThis.__locked = null;
            (async () => {
              const resp = new Response(new Blob(["hello"]));
              let out = "";
              for await (const chunk of resp.body) {
                out += new TextDecoder().decode(chunk);
              }
              globalThis.__out = out;
              globalThis.__locked = resp.body.locked;
            })();
          "#,
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let out = host.window.exec_script("globalThis.__out").unwrap();
    assert_eq!(get_string(host.window.heap(), out), "hello");
    let locked = host.window.exec_script("globalThis.__locked").unwrap();
    assert_eq!(locked, Value::Bool(false));

    Ok(())
  }

  #[test]
  fn readable_stream_values_prevent_cancel_controls_cancellation() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());
    let env = WindowFetchEnv::for_document(
      Arc::new(StaticOkFetcher),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            globalThis.__firstCancel = 0;
            globalThis.__remCancel = 0;
            globalThis.__firstKeep = 0;
            globalThis.__remKeep = 0;
            (async () => {
              const big = "a".repeat(70000);

              const respCancel = new Response(new Blob([big]));
              let firstCancel = 0;
              for await (const chunk of respCancel.body) {
                firstCancel = chunk.length;
                break;
              }
              const readerCancel = respCancel.body.getReader();
              let remCancel = 0;
              while (true) {
                const r = await readerCancel.read();
                if (r.done) break;
                remCancel += r.value.length;
              }
              readerCancel.releaseLock();

              const respKeep = new Response(new Blob([big]));
              let firstKeep = 0;
              for await (const chunk of respKeep.body.values({ preventCancel: true })) {
                firstKeep = chunk.length;
                break;
              }
              const readerKeep = respKeep.body.getReader();
              let remKeep = 0;
              while (true) {
                const r = await readerKeep.read();
                if (r.done) break;
                remKeep += r.value.length;
              }
              readerKeep.releaseLock();

              globalThis.__firstCancel = firstCancel;
              globalThis.__remCancel = remCancel;
              globalThis.__firstKeep = firstKeep;
              globalThis.__remKeep = remKeep;
            })();
          "#,
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let first_cancel = host.window.exec_script("globalThis.__firstCancel").unwrap();
    let rem_cancel = host.window.exec_script("globalThis.__remCancel").unwrap();
    let first_keep = host.window.exec_script("globalThis.__firstKeep").unwrap();
    let rem_keep = host.window.exec_script("globalThis.__remKeep").unwrap();

    let first_cancel = number_to_u64(first_cancel).unwrap();
    let rem_cancel = number_to_u64(rem_cancel).unwrap();
    let first_keep = number_to_u64(first_keep).unwrap();
    let rem_keep = number_to_u64(rem_keep).unwrap();

    let expected_first = 64 * 1024;
    let expected_remaining = 70000 - expected_first;
    assert_eq!(first_cancel, expected_first);
    assert_eq!(rem_cancel, 0);
    assert_eq!(first_keep, expected_first);
    assert_eq!(rem_keep, expected_remaining);

    Ok(())
  }

  #[test]
  fn fetch_string_body_sets_content_type() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          "fetch('https://example.invalid/upload', { method: 'POST', body: 'hi' });",
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let captured = fetcher.take().expect("expected fetch_http_request call");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.url, "https://example.invalid/upload");
    assert_eq!(captured.body.as_deref(), Some(b"hi".as_slice()));
    assert_eq!(
      header_value(&captured.headers, "content-type"),
      Some("text/plain;charset=UTF-8"),
      "headers={:?}",
      captured.headers
    );

    Ok(())
  }

  #[test]
  fn fetch_data_view_body_sends_visible_bytes() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            const buf = new ArrayBuffer(4);
            const u8 = new Uint8Array(buf);
            u8[0] = 10;
            u8[1] = 11;
            u8[2] = 12;
            u8[3] = 13;
            const view = new DataView(buf, 1, 2);
            fetch('https://example.invalid/dataview', { method: 'POST', body: view });
          "#,
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let captured = fetcher.take().expect("expected fetch_http_request call");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.url, "https://example.invalid/dataview");
    assert_eq!(captured.body.as_deref(), Some(&[11u8, 12][..]));
    assert_eq!(
      header_value(&captured.headers, "content-type"),
      None,
      "headers={:?}",
      captured.headers
    );

    Ok(())
  }

  #[test]
  fn fetch_typed_array_body_sends_visible_bytes() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            const buf = new ArrayBuffer(6);
            const u8 = new Uint8Array(buf);
            u8[0] = 0;
            u8[1] = 1;
            u8[2] = 2;
            u8[3] = 3;
            u8[4] = 4;
            u8[5] = 5;
            const view = new Uint16Array(buf, 2, 1);
            fetch('https://example.invalid/u16', { method: 'POST', body: view });
          "#,
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let captured = fetcher.take().expect("expected fetch_http_request call");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.url, "https://example.invalid/u16");
    assert_eq!(captured.body.as_deref(), Some(&[2u8, 3][..]));
    assert_eq!(
      header_value(&captured.headers, "content-type"),
      None,
      "headers={:?}",
      captured.headers
    );

    Ok(())
  }

  #[test]
  fn fetch_blob_body_sends_bytes_and_sets_content_type() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
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
  fn fetch_file_body_sends_bytes_and_sets_content_type() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          "fetch('https://example.invalid/upload', { method: 'POST', body: new File(['hi'], 'x.txt', { type: 'text/plain' }) });",
        )
        .unwrap();
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
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
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
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            const fd = new FormData();
            fd.append('a', 'b');
            fd.append('file', new Blob(['hi'], { type: 'text/plain' }), 'f.txt');
            fetch('https://example.invalid/multipart', { method: 'POST', body: fd });
          "#,
        )
        .unwrap();
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
  fn fetch_readable_stream_body_sends_bytes() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            const stream = new ReadableStream({
              start(c) {
                c.enqueue(new Uint8Array([1, 2, 3]));
                c.enqueue(new Uint8Array([4, 5]));
                c.close();
              },
            });
            fetch("https://example.invalid/post", { method: "POST", body: stream });
          "#,
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let captured = fetcher.take().expect("expected fetch_http_request call");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.url, "https://example.invalid/post");
    assert_eq!(captured.body.as_deref(), Some(&[1, 2, 3, 4, 5][..]));
    Ok(())
  }

  #[test]
  fn fetch_readable_stream_body_too_large_rejects() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let mut limits = WebFetchLimits::default();
    limits.max_request_body_bytes = 3;
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    )
    .with_limits(limits);
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            globalThis.__err = null;
            (async () => {
              const stream = new ReadableStream({
                start(c) {
                  c.enqueue(new Uint8Array([1, 2]));
                  c.enqueue(new Uint8Array([3, 4])); // total 4 > limit 3
                  c.close();
                },
              });
              try {
                await fetch("https://example.invalid/post", { method: "POST", body: stream });
              } catch (e) {
                globalThis.__err = e && e.message ? e.message : String(e);
              }
            })();
          "#,
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(
      fetcher.take().is_none(),
      "expected no fetch_http_request call for oversized body"
    );
    let err_val = host.window.exec_script("globalThis.__err").unwrap();
    assert_eq!(get_string(host.window.heap(), err_val), FETCH_BODY_TOO_LONG_ERROR);
    Ok(())
  }

  #[test]
  fn fetch_reusing_stream_body_for_two_requests_rejects_second() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher = Arc::new(CaptureHttpRequestFetcher::default());
    let env = WindowFetchEnv::for_document(
      fetcher.clone(),
      Some("https://example.invalid/".to_string()),
    );
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      window
        .exec_script_with_host_and_hooks(
          host_ctx,
          &mut hooks,
          r#"
            globalThis.__err = null;
            (async () => {
              const stream = new ReadableStream({
                start(c) {
                  c.enqueue(new Uint8Array([9, 8]));
                  c.close();
                },
              });
              const r1 = new Request("https://example.invalid/one", { method: "POST", body: stream });
              const r2 = new Request("https://example.invalid/two", { method: "POST", body: stream });
              await fetch(r1);
              try {
                await fetch(r2);
              } catch (e) {
                globalThis.__err = e && e.message ? e.message : String(e);
              }
            })();
          "#,
        )
        .unwrap();
    }

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let captured = fetcher.take().expect("expected fetch_http_request call");
    assert_eq!(captured.url, "https://example.invalid/one");
    assert_eq!(captured.body.as_deref(), Some(&[9, 8][..]));

    let err_val = host.window.exec_script("globalThis.__err").unwrap();
    assert_eq!(get_string(host.window.heap(), err_val), "Request body is already used");
    Ok(())
  }

  fn value_to_utf8_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  fn object_url_id(url: &str) -> u64 {
    let (_, id_str) = url
      .rsplit_once('/')
      .unwrap_or_else(|| panic!("expected object URL to contain '/' separator: {url:?}"));
    id_str
      .parse::<u64>()
      .unwrap_or_else(|_| panic!("expected object URL id to be u64: {url:?}"))
  }

  #[test]
  fn fetch_blob_object_url_round_trip_and_revoke() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    // Fetcher should be bypassed for `blob:` object URLs.
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let _bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };

    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      let result = window.exec_script_with_host_and_hooks(
        host_ctx,
        &mut hooks,
        r#"
            globalThis.__u1 = URL.createObjectURL(new Blob(['hi'], { type: 'text/plain' }));
            globalThis.__u2 = URL.createObjectURL(new Blob(['bye']));
             globalThis.__p1 = fetch(globalThis.__u1).then((r) => {
               globalThis.__ct = r.headers.get('content-type');
               return r.text();
             });
           "#,
       );
       if let Some(err) = hooks.finish(window.heap_mut()) {
         return Err(err);
       }
       result.map_err(|e| crate::error::Error::Other(e.to_string()))?;
     }

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let u1_val = host.window.exec_script("globalThis.__u1").unwrap();
    let u1 = value_to_utf8_string(host.window.heap(), u1_val);
    let u2_val = host.window.exec_script("globalThis.__u2").unwrap();
    let u2 = value_to_utf8_string(host.window.heap(), u2_val);

    assert!(u1.starts_with("blob:https://example.invalid/"), "u1={u1:?}");
    assert!(u2.starts_with("blob:https://example.invalid/"), "u2={u2:?}");
    assert!(object_url_id(&u2) > object_url_id(&u1));

    let ct_val = host.window.exec_script("globalThis.__ct").unwrap();
    let ct = value_to_utf8_string(host.window.heap(), ct_val);
    assert_eq!(ct, "text/plain");

    let promise_value = host.window.exec_script("globalThis.__p1").unwrap();
    let Value::Object(promise_obj) = promise_value else {
      panic!("expected promise object, got {promise_value:?}");
    };
    assert_eq!(
      host.window.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Fulfilled
    );
    let Some(result_value) = host.window.heap().promise_result(promise_obj).unwrap() else {
      panic!("fetch promise missing result");
    };
    assert_eq!(value_to_utf8_string(host.window.heap(), result_value), "hi");

    // Revoke + verify subsequent fetch rejects.
    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      let result = window.exec_script_with_host_and_hooks(
        host_ctx,
        &mut hooks,
        r#"
             URL.revokeObjectURL(globalThis.__u1);
             globalThis.__p2 = fetch(globalThis.__u1);
             // Avoid leaking the second URL in the process-global registry.
             URL.revokeObjectURL(globalThis.__u2);
           "#,
       );
       if let Some(err) = hooks.finish(window.heap_mut()) {
         return Err(err);
       }
       result.map_err(|e| crate::error::Error::Other(e.to_string()))?;
     }

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );

    let promise_value = host.window.exec_script("globalThis.__p2").unwrap();
    let Value::Object(promise_obj) = promise_value else {
      panic!("expected promise object, got {promise_value:?}");
    };
    assert_eq!(
      host.window.heap().promise_state(promise_obj).unwrap(),
      PromiseState::Rejected
    );
    let Some(err_value) = host.window.heap().promise_result(promise_obj).unwrap() else {
      panic!("fetch rejection missing reason");
    };
    let Value::Object(err_obj) = err_value else {
      panic!("expected rejection reason object, got {err_value:?}");
    };

    // Assert the rejection reason is a TypeError.
    let (vm, _realm, heap) = host.window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let name_key = alloc_key(&mut scope, "name").unwrap();
    let name_val = vm.get(&mut scope, err_obj, name_key).unwrap();
    assert_eq!(value_to_utf8_string(scope.heap(), name_val), "TypeError");

    Ok(())
  }

  #[test]
  fn fetch_abort_between_network_task_and_settlement_microtask_rejects() -> crate::error::Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EventLoopHost>::with_clock(clock);
    let mut host = EventLoopHost::new_with_js_execution_options(JsExecutionOptions::default());

    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StaticOkFetcher);
    let env = WindowFetchEnv::for_document(fetcher, Some("https://example.invalid/".to_string()));
    let bindings = {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_fetch_bindings_with_guard::<EventLoopHost>(vm, realm, heap, env)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
    };
    let env_id = bindings.env_id();

    let promise = {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      let result = window.exec_script_with_host_and_hooks(
        host_ctx,
        &mut hooks,
        r#"(function(){
             const controller = new AbortController();
             const p = fetch('https://example.invalid/ok', { signal: controller.signal });
             // Ensure the eventual rejection doesn't trigger `unhandledrejection` tasks during the test.
             p.catch(() => {});
             globalThis.__fetch_abort_controller = controller;
             return p;
           })()"#,
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result.map_err(|e| crate::error::Error::Other(e.to_string()))?
    };
    let promise_root = host
      .window
      .heap_mut()
      .add_root(promise)
      .map_err(|e| crate::error::Error::Other(e.to_string()))?;

    // Run the networking task but intentionally stop before executing any microtasks (including the
    // fetch completion microtask).
    let mut run_state = event_loop.new_run_state(RunLimits {
      max_tasks: 1,
      max_microtasks: 0,
      max_wall_time: None,
    });
    let outcome = event_loop.run_next_task_limited(&mut host, &mut run_state)?;
    assert!(
      matches!(
        outcome,
        RunNextTaskLimitedOutcome::Stopped(RunUntilIdleStopReason::MaxMicrotasks { .. })
      ),
      "expected to run networking task but skip microtasks, got {outcome:?}"
    );

    // The promise should still be pending because we skipped microtasks.
    let promise_obj = {
      let heap = host.window.heap();
      let promise_value = heap.get_root(promise_root).unwrap_or(Value::Undefined);
      let Value::Object(promise_obj) = promise_value else {
        return Err(crate::error::Error::Other(
          "expected fetch() to return a Promise object".to_string(),
        ));
      };
      let state = heap
        .promise_state(promise_obj)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      assert_eq!(state, PromiseState::Pending);
      promise_obj
    };

    // The networking task has already stored a backing response; it must be removed if we reject
    // due to abort before settlement.
    assert_eq!(
      with_env_state(env_id, host.window.heap(), |state| Ok(state.responses.len()))
        .map_err(|e| crate::error::Error::Other(e.to_string()))?,
      1
    );

    // Abort after the networking task has completed, but before the settlement microtask runs.
    {
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let EventLoopHost { host_ctx, window } = &mut host;
      let result = window.exec_script_with_host_and_hooks(
        host_ctx,
        &mut hooks,
        "globalThis.__fetch_abort_controller.abort('reason');",
      );
      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }
      result.map_err(|e| crate::error::Error::Other(e.to_string()))?;
    }

    // Now run the microtask checkpoint; the completion microtask must observe the abort and reject.
    event_loop.perform_microtask_checkpoint(&mut host)?;

    {
      let heap = host.window.heap();
      let state = heap
        .promise_state(promise_obj)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?;
      assert_eq!(state, PromiseState::Rejected);

      let Some(result) = heap
        .promise_result(promise_obj)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
      else {
        return Err(crate::error::Error::Other(
          "expected rejected fetch promise to have a result".to_string(),
        ));
      };
      let Value::String(reason_s) = result else {
        return Err(crate::error::Error::Other(
          "expected fetch rejection reason to be a string".to_string(),
        ));
      };
      let reason = heap
        .get_string(reason_s)
        .map_err(|e| crate::error::Error::Other(e.to_string()))?
        .to_utf8_lossy();
      assert_eq!(reason, "reason");
    }

    assert_eq!(
      with_env_state(env_id, host.window.heap(), |state| Ok(state.responses.len()))
        .map_err(|e| crate::error::Error::Other(e.to_string()))?,
      0
    );

    host.window.heap_mut().remove_root(promise_root);
    drop(bindings);
    Ok(())
  }

  #[test]
  fn fetch_completion_microtask_respects_vm_fuel_budget() -> crate::error::Result<()> {
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
      let mut hooks = VmJsEventLoopHooks::<EventLoopHost>::new_with_host(&mut host)?;
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
  fn webidl_host_slot_available_in_fetch_completion_thenable_assimilation(
  ) -> crate::error::Result<()> {
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
      fn vm_host_and_window_realm(
        &mut self,
      ) -> crate::error::Result<(&mut dyn vm_js::VmHost, &mut WindowRealm)> {
        let DispatchEventLoopHost {
          host_ctx, window, ..
        } = self;
        Ok((host_ctx, window))
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
      scope
        .push_root(Value::Object(global))
        .expect("push root global");

      let name_s = scope.alloc_string("__webidl_dispatch").unwrap();
      scope.push_root(Value::String(name_s)).unwrap();
      let func = scope
        .alloc_native_function(call_id, None, name_s, 0)
        .unwrap();
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
      let mut hooks = VmJsEventLoopHooks::<DispatchEventLoopHost>::new_with_host(&mut host)?;
      hooks.set_event_loop(&mut event_loop);
      let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
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

  #[test]
  fn window_host_readable_stream_pipe_to_and_pipe_through_pump() -> crate::error::Result<()> {
    let dom = crate::dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", Arc::new(StaticOkFetcher))?;

    host.exec_script(
      r#"
      globalThis.__out = "";
      globalThis.__err = "";
      globalThis.__closed = false;
      globalThis.__chunks = [];

      const src = new Response("ok").body;
      const dest = {
        getWriter() {
          return {
            write(chunk) {
              try { globalThis.__chunks.push(new TextDecoder().decode(chunk)); }
              catch (e) { globalThis.__err = String(e && e.message || e); }
              return Promise.resolve();
            },
            close() {
              globalThis.__closed = true;
              return Promise.resolve();
            }
          };
        }
      };

      src.pipeTo(dest)
        .then(() => { globalThis.__out = globalThis.__chunks.join(""); })
        .catch(e => { globalThis.__err = String(e && e.message || e); });

      globalThis.__out2 = "";
      globalThis.__err2 = "";
      globalThis.__closed2 = false;
      globalThis.__chunks2 = [];

      const dest2 = {
        getWriter() {
          return {
            write(chunk) {
              try { globalThis.__chunks2.push(new TextDecoder().decode(chunk)); }
              catch (e) { globalThis.__err2 = String(e && e.message || e); }
              return Promise.resolve();
            },
            close() {
              globalThis.__closed2 = true;
              return Promise.resolve();
            }
          };
        }
      };

      new Response("ok").body
        .pipeTo(dest2, { preventClose: true })
        .then(() => { globalThis.__out2 = globalThis.__chunks2.join(""); })
        .catch(e => { globalThis.__err2 = String(e && e.message || e); });

      globalThis.__through_ok = false;
      globalThis.__through_out = "";
      globalThis.__through_err = "";
      globalThis.__through_closed = false;
      globalThis.__through_throw = false;
      globalThis.__through_chunks = [];

      const throughReadable = {};
      const throughDest = {
        getWriter() {
          return {
            write(chunk) {
              try { globalThis.__through_chunks.push(new TextDecoder().decode(chunk)); }
              catch (e) { globalThis.__through_err = String(e && e.message || e); }
              return Promise.resolve();
            },
            close() {
              globalThis.__through_closed = true;
              globalThis.__through_out = globalThis.__through_chunks.join("");
              return Promise.resolve();
            }
          };
        }
      };
      const transform = { writable: throughDest, readable: throughReadable };
      const returned = new Response("ok").body.pipeThrough(transform);
      globalThis.__through_ok = returned === throughReadable;

      try { new Response("ok").body.pipeThrough(null); }
      catch (e) { globalThis.__through_throw = e instanceof TypeError; }
      "#,
    )?;

    let _ = host.run_until_idle(RunLimits {
      max_tasks: 10,
      max_microtasks: 100,
      max_wall_time: Some(Duration::from_secs(5)),
    })?;

    assert_eq!(get_global_prop_utf8(&mut host, "__err").unwrap_or_default(), "");
    assert_eq!(get_global_prop_utf8(&mut host, "__out").unwrap_or_default(), "ok");
    assert!(matches!(get_global_prop(&mut host, "__closed"), Value::Bool(true)));

    assert_eq!(get_global_prop_utf8(&mut host, "__err2").unwrap_or_default(), "");
    assert_eq!(get_global_prop_utf8(&mut host, "__out2").unwrap_or_default(), "ok");
    assert!(matches!(
      get_global_prop(&mut host, "__closed2"),
      Value::Bool(false)
    ));

    assert_eq!(
      get_global_prop_utf8(&mut host, "__through_err").unwrap_or_default(),
      ""
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__through_out").unwrap_or_default(),
      "ok"
    );
    assert!(matches!(
      get_global_prop(&mut host, "__through_ok"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__through_closed"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__through_throw"),
      Value::Bool(true)
    ));

    Ok(())
  }
}
