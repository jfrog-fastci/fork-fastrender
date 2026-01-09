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

use crate::error::Error;
use crate::js::event_loop::TaskSource;
use crate::js::runtime::{current_event_loop_mut, with_event_loop};
use crate::js::window_realm::{WindowRealm, WindowRealmHost};
use crate::render_control;
use crate::resource::web_fetch::{
  execute_web_fetch, Body, Headers as CoreHeaders, HeadersGuard, Request as CoreRequest, Response as CoreResponse,
  RequestCredentials, WebFetchExecutionContext, WebFetchError, WebFetchLimits,
};
use crate::resource::{origin_from_url, DocumentOrigin, FetchDestination, ReferrerPolicy, ResourceFetcher};
use std::char::decode_utf16;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use vm_js::{
  Budget, ExecutionContext, GcObject, Heap, Job, JobCallback, PropertyDescriptor, PropertyKey, PropertyKind,
  NativeFunctionId, Realm, RealmId, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext,
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

#[derive(Clone)]
pub struct WindowFetchEnv {
  pub fetcher: Arc<dyn ResourceFetcher>,
  pub document_url: Option<String>,
  pub document_origin: Option<DocumentOrigin>,
  pub referrer_policy: ReferrerPolicy,
}

impl WindowFetchEnv {
  pub fn for_document(fetcher: Arc<dyn ResourceFetcher>, document_url: Option<String>) -> Self {
    let document_origin = document_url.as_deref().and_then(origin_from_url);
    Self {
      fetcher,
      document_url,
      document_origin,
      referrer_policy: ReferrerPolicy::default(),
    }
  }
}

struct EnvState {
  env: WindowFetchEnv,
  promise_executor_call: NativeFunctionId,
  next_id: u64,
  owned_headers: HashMap<u64, CoreHeaders>,
  requests: HashMap<u64, CoreRequest>,
  responses: HashMap<u64, CoreResponse>,
}

impl EnvState {
  fn new(env: WindowFetchEnv, promise_executor_call: NativeFunctionId) -> Self {
    Self {
      env,
      promise_executor_call,
      next_id: 1,
      owned_headers: HashMap::new(),
      requests: HashMap::new(),
      responses: HashMap::new(),
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
static DEFAULT_FETCH_LIMITS: OnceLock<WebFetchLimits> = OnceLock::new();

fn envs() -> &'static Mutex<HashMap<u64, EnvState>> {
  ENVS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn default_fetch_limits() -> &'static WebFetchLimits {
  DEFAULT_FETCH_LIMITS.get_or_init(WebFetchLimits::default)
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
const FETCH_BODY_TOO_LONG_ERROR: &str = "fetch body string exceeds maximum length";
const FETCH_CREDENTIALS_TOO_LONG_ERROR: &str = "Request.credentials exceeds maximum length";
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

fn create_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  ctor: GcObject,
  message: &str,
) -> Result<Value, VmError> {
  let msg = scope.alloc_string(message)?;
  scope.push_root(Value::String(msg))?;
  vm.construct_with_host(
    scope,
    host,
    Value::Object(ctor),
    &[Value::String(msg)],
    Value::Object(ctor),
  )
}

fn create_type_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  message: &str,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "TypeError requires intrinsics (create a Realm first)",
  ))?;
  create_error(vm, scope, host, intr.type_error(), message)
}

fn create_syntax_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  message: &str,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "SyntaxError requires intrinsics (create a Realm first)",
  ))?;
  create_error(vm, scope, host, intr.syntax_error(), message)
}

fn throw_type_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  message: &str,
) -> VmError {
  match create_type_error(vm, scope, host, message) {
    Ok(err) => VmError::Throw(err),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn throw_syntax_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  message: &str,
) -> VmError {
  match create_syntax_error(vm, scope, host, message) {
    Ok(err) => VmError::Throw(err),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn map_web_fetch_error_to_throw(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHostHooks,
  err: WebFetchError,
) -> VmError {
  match err {
    WebFetchError::BodyInvalidJson(e) => throw_syntax_error(vm, scope, host, &e.to_string()),
    other => throw_type_error(vm, scope, host, &other.to_string()),
  }
}

fn callback_budget_from_render_deadline() -> Budget {
  let deadline = render_control::root_deadline().and_then(|d| d.remaining_timeout());
  let deadline = deadline.and_then(|remaining| Instant::now().checked_add(remaining));
  Budget {
    fuel: Some(1_000_000),
    deadline,
    check_time_every: 100,
  }
}

fn vm_error_to_event_loop_error(heap: &mut Heap, err: VmError) -> Error {
  match err {
    VmError::Throw(value) => {
      if let Value::String(s) = value {
        if let Ok(js) = heap.get_string(s) {
          // Converting a UTF-16 JS string to a Rust `String` allocates in the host. Keep this
          // bounded so hostile scripts cannot force large host allocations via `throw "..."`.
          const MAX_THROWN_STRING_CODE_UNITS: usize = 4096;
          if js.len_code_units() <= MAX_THROWN_STRING_CODE_UNITS {
            return Error::Other(js.to_utf8_lossy());
          }
        }
      }

      if let Value::Object(obj) = value {
        let mut scope = heap.scope();
        let _ = scope.push_root(Value::Object(obj));

        let mut get_prop_str = |name: &str| -> Option<String> {
          let key_s = scope.alloc_string(name).ok()?;
          scope.push_root(Value::String(key_s)).ok()?;
          let key = PropertyKey::from_string(key_s);
          let value = scope
            .heap()
            .object_get_own_data_property_value(obj, &key)
            .ok()?
            .unwrap_or(Value::Undefined);
          match value {
            Value::String(s) => {
              const MAX_THROWN_STRING_CODE_UNITS: usize = 4096;
              let js = scope.heap().get_string(s).ok()?;
              if js.len_code_units() > MAX_THROWN_STRING_CODE_UNITS {
                return None;
              }
              Some(js.to_utf8_lossy())
            }
            _ => None,
          }
        };

        let name = get_prop_str("name");
        let message = get_prop_str("message");
        if let (Some(name), Some(message)) = (name, message) {
          if !message.is_empty() {
            return Error::Other(format!("{name}: {message}"));
          }
          return Error::Other(name);
        }
      }

      Error::Other("uncaught exception".to_string())
    }
    VmError::Syntax(diags) => Error::Other(format!("syntax error: {diags:?}")),
    other => Error::Other(other.to_string()),
  }
}

struct HeapRootContext<'a> {
  heap: &'a mut Heap,
}

impl VmJobContext for HeapRootContext<'_> {
  fn call(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("HeapRootContext::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("HeapRootContext::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id);
  }
}

struct WindowRealmJobContext<'a> {
  window_realm: &'a mut WindowRealm,
  realm: Option<RealmId>,
}

impl<'a> WindowRealmJobContext<'a> {
  fn new(window_realm: &'a mut WindowRealm, realm: Option<RealmId>) -> Self {
    Self { window_realm, realm }
  }
}

impl VmJobContext for WindowRealmJobContext<'_> {
  fn call(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let (vm, heap) = self.window_realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = vm.execution_context_guard(ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.call_with_host(&mut scope, host, callee, this, args)
    } else {
      vm.call_with_host(&mut scope, host, callee, this, args)
    }
  }

  fn construct(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let (vm, heap) = self.window_realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = vm.execution_context_guard(ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.construct_with_host(&mut scope, host, callee, args, new_target)
    } else {
      vm.construct_with_host(&mut scope, host, callee, args, new_target)
    }
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.window_realm.heap_mut().add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.window_realm.heap_mut().remove_root(id);
  }
}

struct VmJsEventLoopHooks<Host: WindowRealmHost + 'static> {
  pending_discard: Vec<Job>,
  enqueue_error: Option<Error>,
  _marker: std::marker::PhantomData<fn() -> Host>,
}

impl<Host: WindowRealmHost + 'static> VmJsEventLoopHooks<Host> {
  fn new() -> Self {
    Self {
      pending_discard: Vec::new(),
      enqueue_error: None,
      _marker: std::marker::PhantomData,
    }
  }

  fn finish(mut self, heap: &mut Heap) -> Option<Error> {
    if !self.pending_discard.is_empty() {
      let mut ctx = HeapRootContext { heap };
      for job in self.pending_discard.drain(..) {
        job.discard(&mut ctx);
      }
    }
    self.enqueue_error.take()
  }
}

impl<Host: WindowRealmHost + 'static> VmHostHooks for VmJsEventLoopHooks<Host> {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    if self.enqueue_error.is_some() {
      self.pending_discard.push(job);
      return;
    }

    let job_cell: std::rc::Rc<std::cell::RefCell<Option<Job>>> =
      std::rc::Rc::new(std::cell::RefCell::new(Some(job)));
    let job_cell_for_closure = std::rc::Rc::clone(&job_cell);

    let enqueue_result: crate::error::Result<()> = (|| {
      let Some(event_loop) = current_event_loop_mut::<Host>() else {
        return Err(Error::Other(
          "vm-js Promise job enqueued without an active EventLoop".to_string(),
        ));
      };

      event_loop.queue_microtask(move |host, event_loop| {
        let Some(job) = job_cell_for_closure.borrow_mut().take() else {
          return Ok(());
        };

        let window_realm = host.window_realm();
        window_realm.reset_interrupt();

        with_event_loop(event_loop, || {
          let vm = window_realm.vm_mut();
          vm.set_budget(callback_budget_from_render_deadline());
          let tick_result = vm.tick();

          let mut hooks = VmJsEventLoopHooks::<Host>::new();
          let job_result = tick_result.and_then(|_| {
            let mut ctx = WindowRealmJobContext::new(window_realm, realm);
            job.run(&mut ctx, &mut hooks)
          });

          window_realm
            .vm_mut()
            .set_budget(Budget::unlimited(100));

          if let Some(err) = hooks.finish(window_realm.heap_mut()) {
            return Err(err);
          }

          job_result
            .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
            .map(|_| ())
        })
      })
    })();

    if let Err(err) = enqueue_result {
      if let Some(job) = job_cell.borrow_mut().take() {
        self.pending_discard.push(job);
      }
      self.enqueue_error = Some(err);
    }
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> Result<Value, VmError> {
    ctx.call(
      self,
      Value::Object(callback.callback_object()),
      this_argument,
      arguments,
    )
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
  host: &mut dyn VmHostHooks,
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
  let promise = vm.construct_with_host(
    scope,
    host,
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
  host: &mut dyn VmHostHooks,
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
        .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, err))?;
      return Ok(());
    }
  }

  // Sequence-of-pairs form: treat Arrays as `sequence<sequence<ByteString>>`.
  if scope.heap().object_prototype(obj)? == Some(array_proto) {
    // Read `.length` as a u32.
    let length_key = alloc_key(scope, "length")?;
    let len_value = vm.get(scope, obj, length_key)?;
    let len_u64 = number_to_u64(len_value)?;
    let len: usize = len_u64
      .try_into()
      .map_err(|_| throw_type_error(vm, scope, host, "Headers init array too large"))?;

    let mut sequence: Vec<[String; 2]> = Vec::with_capacity(len);
    for idx in 0..len {
      let key = alloc_key(scope, &idx.to_string())?;
      let entry = vm.get(scope, obj, key)?;
      let Value::Object(entry_obj) = entry else {
        return Err(throw_type_error(vm, scope, host, "Invalid Headers init sequence item"));
      };
      if scope.heap().object_prototype(entry_obj)? != Some(array_proto) {
        return Err(throw_type_error(vm, scope, host, "Invalid Headers init sequence item"));
      }
      let entry_len_key = alloc_key(scope, "length")?;
      let entry_len = vm.get(scope, entry_obj, entry_len_key)?;
      let entry_len = number_to_u64(entry_len)?;
      if entry_len != 2 {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          "Invalid Headers init sequence item length",
        ));
      }
      let k0 = alloc_key(scope, "0")?;
      let k1 = alloc_key(scope, "1")?;
      let name_val = vm.get(scope, entry_obj, k0)?;
      let value_val = vm.get(scope, entry_obj, k1)?;
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
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, err))?;
    return Ok(());
  }

  // Record form: iterate own keys in `[[OwnPropertyKeys]]` order.
  let keys = scope.heap().ordinary_own_property_keys(obj)?;
  let mut pairs: Vec<(String, String)> = Vec::new();
  for key in keys {
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
    let value_val = vm.get(scope, obj, key)?;
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
    .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host, err))?;

  // Prevent unused warning for env_id (future: cross-env copy checks).
  let _ = env_id;
  Ok(())
}

fn headers_append_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
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
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host_hooks, err))?;
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn headers_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
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
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host_hooks, err))?;
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn headers_delete_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
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
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host_hooks, err))?;
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn headers_has_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
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
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host_hooks, err))
  })?;
  Ok(Value::Bool(has))
}

fn headers_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
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
      .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host_hooks, err))
  })?;
  match value {
    Some(v) => {
      let s = scope.alloc_string(&v)?;
      Ok(Value::String(s))
    }
    None => Ok(Value::Null),
  }
}

fn headers_for_each_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
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
    vm.call_with_host(
      scope,
      host_hooks,
      callback,
      this_arg,
      &[Value::String(value_s), Value::String(name_s), Value::Object(headers_obj)],
    )?;
  }

  Ok(Value::Undefined)
}

fn headers_ctor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(throw_type_error(vm, scope, host_hooks, "Illegal constructor"))
}

fn headers_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;

  let mut core = CoreHeaders::new_with_guard(HeadersGuard::None);
  if let Some(init) = args.get(0).copied() {
    // Fill before installing into the env state so errors don't leave partial state behind.
    fill_headers_from_init(vm, scope, host_hooks, env_id, &mut core, init)?;
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

fn request_ctor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(throw_type_error(vm, scope, host_hooks, "Illegal constructor"))
}

fn request_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let init = args.get(1).copied().unwrap_or(Value::Undefined);

  let mut request = if let Some((other_env_id, other_request_id)) =
    request_info_from_value(scope, input)
  {
    with_env_state(other_env_id, |state| {
      state
        .requests
        .get(&other_request_id)
        .cloned()
        .ok_or(VmError::TypeError("Request: invalid backing request"))
      })?
  } else {
    let url = to_rust_string_limited(
      scope.heap_mut(),
      input,
      default_fetch_limits().max_url_bytes,
      FETCH_URL_TOO_LONG_ERROR,
    )?;
    CoreRequest::new("GET", url)
  };

  if !matches!(init, Value::Undefined | Value::Null) {
    let Value::Object(init_obj) = init else {
      return Err(VmError::TypeError("Request init must be an object"));
    };
    let method_key = alloc_key(scope, "method")?;
    let method_val = vm.get(scope, init_obj, method_key)?;
    if !matches!(method_val, Value::Undefined | Value::Null) {
      request.method = to_rust_string_limited(
        scope.heap_mut(),
        method_val,
        default_fetch_limits().max_url_bytes,
        FETCH_METHOD_TOO_LONG_ERROR,
      )?;
    }
    let headers_key = alloc_key(scope, "headers")?;
    let headers_val = vm.get(scope, init_obj, headers_key)?;
    if !matches!(headers_val, Value::Undefined | Value::Null) {
      // `RequestInit.headers` replaces the existing header list.
      let mut headers =
        CoreHeaders::new_with_guard_and_limits(request.headers.guard(), request.headers.limits());
      fill_headers_from_init(vm, scope, host_hooks, env_id, &mut headers, headers_val)?;
      request.headers = headers;
    }

    let credentials_key = alloc_key(scope, "credentials")?;
    let credentials_val = vm.get(scope, init_obj, credentials_key)?;
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
            host_hooks,
            "Request.credentials must be \"omit\", \"same-origin\", or \"include\"",
          ));
        }
      };
    }

    let body_key = alloc_key(scope, "body")?;
    let body_val = vm.get(scope, init_obj, body_key)?;
    if !matches!(body_val, Value::Undefined | Value::Null) {
      let bytes = to_rust_string_limited(
        scope.heap_mut(),
        body_val,
        request.headers.limits().max_request_body_bytes,
        FETCH_BODY_TOO_LONG_ERROR,
      )?
      .into_bytes();
      let body = Body::new_with_limits(bytes, request.headers.limits())
        .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host_hooks, err))?;
      request.body = Some(body);
    }
  }

  if request.method.eq_ignore_ascii_case("GET") || request.method.eq_ignore_ascii_case("HEAD") {
    if request.body.is_some() {
      return Err(throw_type_error(vm, scope, host_hooks, "Request body is not allowed for GET/HEAD"));
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
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, REQUEST_ID_KEY, Value::Number(request_id as f64), false)?;

  // `method`/`url` as data props (read-only for now).
  let (method, url) = with_env_state(env_id, |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::InvariantViolation("Request state missing"))?;
    Ok((req.method.clone(), req.url.clone()))
  })?;
  let method_s = scope.alloc_string(&method)?;
  let url_s = scope.alloc_string(&url)?;
  set_data_prop(scope, obj, "method", Value::String(method_s), false)?;
  set_data_prop(scope, obj, "url", Value::String(url_s), false)?;

  // `headers` is a live wrapper backed by the request state.
  let headers_obj =
    make_headers_wrapper(scope, env_id, headers_proto, HEADERS_KIND_REQUEST, request_id)?;
  set_data_prop(scope, obj, "headers", Value::Object(headers_obj), false)?;

  Ok(Value::Object(obj))
}

fn request_clone_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Request: illegal invocation"));
  };
  let (env_id, request_id) = request_info_from_this(scope, Value::Object(obj))?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;

  let cloned = with_env_state(env_id, |state| {
    let req = state
      .requests
      .get(&request_id)
      .ok_or(VmError::TypeError("Request: invalid backing request"))?;
    if req.body.as_ref().map_or(false, |b| b.body_used()) {
      return Err(throw_type_error(vm, scope, host_hooks, "Request body is already used"));
    }
    Ok(req.clone())
  })?;

  let new_request_id = with_env_state_mut(env_id, |state| {
    let id = state.alloc_id();
    state.requests.insert(id, cloned);
    Ok(id)
  })?;

  let proto = scope.heap().object_prototype(obj)?.ok_or(VmError::InvariantViolation(
    "Request.prototype missing on instance",
  ))?;
  let obj = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(obj))?;

  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, REQUEST_ID_KEY, Value::Number(new_request_id as f64), false)?;

  let (method, url) = with_env_state(env_id, |state| {
    let req = state
      .requests
      .get(&new_request_id)
      .ok_or(VmError::InvariantViolation("Request state missing"))?;
    Ok((req.method.clone(), req.url.clone()))
  })?;
  let method_s = scope.alloc_string(&method)?;
  let url_s = scope.alloc_string(&url)?;
  set_data_prop(scope, obj, "method", Value::String(method_s), false)?;
  set_data_prop(scope, obj, "url", Value::String(url_s), false)?;

  let headers_obj =
    make_headers_wrapper(scope, env_id, headers_proto, HEADERS_KIND_REQUEST, new_request_id)?;
  set_data_prop(scope, obj, "headers", Value::Object(headers_obj), false)?;

  Ok(Value::Object(obj))
}

fn response_ctor_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(throw_type_error(vm, scope, host_hooks, "Illegal constructor"))
}

fn response_text_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, response_id) = response_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, host_hooks, env_id)?;

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
      vm.call_with_host(scope, host_hooks, cap.resolve, Value::Undefined, &[Value::String(s)])?;
    }
    Err(err) => {
      let err_value = create_type_error(vm, scope, host_hooks, &err.to_string())?;
      vm.call_with_host(scope, host_hooks, cap.reject, Value::Undefined, &[err_value])?;
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
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, response_id) = response_info_from_this(scope, this)?;

  let cap = new_promise_capability_for_env(vm, scope, host_hooks, env_id)?;

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
      vm.call_with_host(scope, host_hooks, cap.resolve, Value::Undefined, &[js_value])?;
    }
    Some(Err(err)) => match err {
      WebFetchError::BodyInvalidJson(e) => {
        let err_value = create_syntax_error(vm, scope, host_hooks, &e.to_string())?;
        vm.call_with_host(scope, host_hooks, cap.reject, Value::Undefined, &[err_value])?;
      }
      other => {
        let err_value = create_type_error(vm, scope, host_hooks, &other.to_string())?;
        vm.call_with_host(scope, host_hooks, cap.reject, Value::Undefined, &[err_value])?;
      }
    },
    None => {
      let err_value = create_type_error(vm, scope, host_hooks, "Response body is null")?;
      vm.call_with_host(scope, host_hooks, cap.reject, Value::Undefined, &[err_value])?;
    }
  }

  Ok(cap.promise)
}

fn response_clone_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
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
      return Err(throw_type_error(vm, scope, host_hooks, "Response body is already used"));
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
  let resp_obj = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(resp_obj))?;

  set_data_prop(
    scope,
    resp_obj,
    ENV_ID_KEY,
    Value::Number(env_id as f64),
    false,
  )?;
  set_data_prop(
    scope,
    resp_obj,
    RESPONSE_ID_KEY,
    Value::Number(new_response_id as f64),
    false,
  )?;

  let (status, ok, url, status_text) = with_env_state(env_id, |state| {
    let r = state
      .responses
      .get(&new_response_id)
      .ok_or(VmError::InvariantViolation("Response state missing"))?;
    Ok((r.status, (200..300).contains(&r.status), r.url.clone(), r.status_text.clone()))
  })?;
  set_data_prop(scope, resp_obj, "status", Value::Number(status as f64), false)?;
  set_data_prop(scope, resp_obj, "ok", Value::Bool(ok), false)?;
  let url_s = scope.alloc_string(&url)?;
  let st_s = scope.alloc_string(&status_text)?;
  set_data_prop(scope, resp_obj, "url", Value::String(url_s), false)?;
  set_data_prop(scope, resp_obj, "statusText", Value::String(st_s), false)?;

  let headers_obj = make_headers_wrapper(
    scope,
    env_id,
    headers_proto,
    HEADERS_KIND_RESPONSE,
    new_response_id,
  )?;
  set_data_prop(scope, resp_obj, "headers", Value::Object(headers_obj), false)?;

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

fn response_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;

  let init = args.get(1).copied().unwrap_or(Value::Undefined);
  let mut status: u16 = 200;
  let mut status_text = String::new();
  let mut headers = CoreHeaders::new_with_guard(HeadersGuard::Response);

  let body = args.get(0).copied().unwrap_or(Value::Undefined);
  let body_bytes = if matches!(body, Value::Undefined | Value::Null) {
    None
  } else {
    Some(
      to_rust_string_limited(
        scope.heap_mut(),
        body,
        headers.limits().max_response_body_bytes,
        FETCH_BODY_TOO_LONG_ERROR,
      )?
      .into_bytes(),
    )
  };

  if !matches!(init, Value::Undefined | Value::Null) {
    let Value::Object(init_obj) = init else {
      return Err(VmError::TypeError("Response init must be an object"));
    };
    let status_key = alloc_key(scope, "status")?;
    let status_val = vm.get(scope, init_obj, status_key)?;
    if let Value::Number(n) = status_val {
      if n.is_finite() && n >= 0.0 && n <= u16::MAX as f64 {
        status = n as u16;
      }
    }
    let status_text_key = alloc_key(scope, "statusText")?;
    let st_val = vm.get(scope, init_obj, status_text_key)?;
    if !matches!(st_val, Value::Undefined | Value::Null) {
      status_text = to_rust_string_limited(
        scope.heap_mut(),
        st_val,
        default_fetch_limits().max_url_bytes,
        FETCH_STATUS_TEXT_TOO_LONG_ERROR,
      )?;
    }
    let headers_key = alloc_key(scope, "headers")?;
    let headers_val = vm.get(scope, init_obj, headers_key)?;
    if !matches!(headers_val, Value::Undefined | Value::Null) {
      fill_headers_from_init(vm, scope, host_hooks, env_id, &mut headers, headers_val)?;
    }
  }

  let mut response = CoreResponse::new(status);
  response.status_text = status_text;
  response.headers = headers;
  if let Some(bytes) = body_bytes {
    response.body = Some(
      crate::resource::web_fetch::Body::new_response(bytes, response.headers.limits())
        .map_err(|e| map_web_fetch_error_to_throw(vm, scope, host_hooks, e))?,
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
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.heap_mut().object_set_prototype(obj, Some(proto))?;

  set_data_prop(scope, obj, ENV_ID_KEY, Value::Number(env_id as f64), false)?;
  set_data_prop(scope, obj, RESPONSE_ID_KEY, Value::Number(response_id as f64), false)?;

  // Data properties.
  let (status, ok, url, status_text) = with_env_state(env_id, |state| {
    let r = state
      .responses
      .get(&response_id)
      .ok_or(VmError::InvariantViolation("Response state missing"))?;
    Ok((r.status, (200..300).contains(&r.status), r.url.clone(), r.status_text.clone()))
  })?;
  set_data_prop(scope, obj, "status", Value::Number(status as f64), false)?;
  set_data_prop(scope, obj, "ok", Value::Bool(ok), false)?;
  let url_s = scope.alloc_string(&url)?;
  let st_s = scope.alloc_string(&status_text)?;
  set_data_prop(scope, obj, "url", Value::String(url_s), false)?;
  set_data_prop(scope, obj, "statusText", Value::String(st_s), false)?;

  // Headers wrapper.
  let headers_obj =
    make_headers_wrapper(scope, env_id, headers_proto, HEADERS_KIND_RESPONSE, response_id)?;
  set_data_prop(scope, obj, "headers", Value::Object(headers_obj), false)?;

  Ok(Value::Object(obj))
}

fn fetch_call<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  host_hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;
  let headers_proto = headers_proto_from_callee(scope, callee)?;
  let response_proto = response_proto_from_callee(scope, callee)?;
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let init = args.get(1).copied().unwrap_or(Value::Undefined);

  // Build request synchronously (invalid init should reject deterministically).
  let mut request =
    if let Some((other_env_id, other_request_id)) = request_info_from_value(scope, input) {
      with_env_state(other_env_id, |state| {
        state
          .requests
          .get(&other_request_id)
          .cloned()
          .ok_or(VmError::TypeError("Request: invalid backing request"))
      })?
    } else {
      let url = to_rust_string_limited(
        scope.heap_mut(),
        input,
        default_fetch_limits().max_url_bytes,
        FETCH_URL_TOO_LONG_ERROR,
      )?;
      let mut request = CoreRequest::new("GET", url);
      request.set_mode(crate::resource::web_fetch::RequestMode::Cors);
       request
     };

  if !matches!(init, Value::Undefined | Value::Null) {
    let Value::Object(init_obj) = init else {
      return Err(VmError::TypeError("Request init must be an object"));
    };
    let method_key = alloc_key(scope, "method")?;
    let method_val = vm.get(scope, init_obj, method_key)?;
    if !matches!(method_val, Value::Undefined | Value::Null) {
      request.method = to_rust_string_limited(
        scope.heap_mut(),
        method_val,
        default_fetch_limits().max_url_bytes,
        FETCH_METHOD_TOO_LONG_ERROR,
      )?;
    }
    let headers_key = alloc_key(scope, "headers")?;
    let headers_val = vm.get(scope, init_obj, headers_key)?;
    if !matches!(headers_val, Value::Undefined | Value::Null) {
      // `RequestInit.headers` replaces the existing header list (Fetch `new Request(input, init)`).
      let mut headers = CoreHeaders::new_with_guard_and_limits(request.headers.guard(), request.headers.limits());
      fill_headers_from_init(vm, scope, host_hooks, env_id, &mut headers, headers_val)?;
      request.headers = headers;
    }

    let credentials_key = alloc_key(scope, "credentials")?;
    let credentials_val = vm.get(scope, init_obj, credentials_key)?;
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
            host_hooks,
            "Request.credentials must be \"omit\", \"same-origin\", or \"include\"",
          ));
        }
      };
    }

    let body_key = alloc_key(scope, "body")?;
    let body_val = vm.get(scope, init_obj, body_key)?;
    if !matches!(body_val, Value::Undefined | Value::Null) {
      let bytes = to_rust_string_limited(
        scope.heap_mut(),
        body_val,
        request.headers.limits().max_request_body_bytes,
        FETCH_BODY_TOO_LONG_ERROR,
      )?
      .into_bytes();
      let body = Body::new_with_limits(bytes, request.headers.limits())
        .map_err(|err| map_web_fetch_error_to_throw(vm, scope, host_hooks, err))?;
      request.body = Some(body);
    }
  }

  // Create a Promise capability for the returned Promise.
  let cap = new_promise_capability_for_env(vm, scope, host_hooks, env_id)?;
  let promise = cap.promise;

  // Resolve/reject later; keep them rooted until settlement.
  let resolve_root = scope.heap_mut().add_root(cap.resolve)?;
  let reject_root = scope.heap_mut().add_root(cap.reject)?;
  let promise_root = scope.heap_mut().add_root(promise)?;

  let Some(event_loop) = current_event_loop_mut::<Host>() else {
    // Reject synchronously.
    scope.heap_mut().remove_root(resolve_root);
    scope.heap_mut().remove_root(reject_root);
    scope.heap_mut().remove_root(promise_root);
    let err = create_type_error(vm, scope, host_hooks, "fetch called without an active EventLoop")?;
    vm.call_with_host(scope, host_hooks, cap.reject, Value::Undefined, &[err])?;
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
          let window_realm = host.window_realm();
          window_realm.reset_interrupt();
          with_event_loop(event_loop, || {
            let vm = window_realm.vm_mut();
            vm.set_budget(callback_budget_from_render_deadline());
            let tick_result = vm.tick();
            let mut hooks = VmJsEventLoopHooks::<Host>::new();
            let call_result = tick_result.and_then(|_| {
              let (vm, heap) = window_realm.vm_and_heap_mut();
              let reject = heap.get_root(reject_root).ok_or(VmError::InvalidHandle)?;
              let mut scope = heap.scope();
              let type_error = create_type_error(vm, &mut scope, &mut hooks, &message)?;
              vm.call_with_host(
                &mut scope,
                &mut hooks,
                reject,
                Value::Undefined,
                &[type_error],
              )?;
              Ok(())
            });

            window_realm.heap_mut().remove_root(resolve_root);
            window_realm.heap_mut().remove_root(reject_root);
            window_realm.heap_mut().remove_root(promise_root);

            window_realm
              .vm_mut()
              .set_budget(Budget::unlimited(100));
            if let Some(err) = hooks.finish(window_realm.heap_mut()) {
              return Err(err);
            }
            call_result
              .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
              .map(|_| ())
          })
        });

        if let Err(queue_err) = queue_result {
          // If we can't even enqueue the rejection microtask, tear down persistent roots now.
          let window_realm = host.window_realm();
          window_realm.heap_mut().remove_root(resolve_root);
          window_realm.heap_mut().remove_root(reject_root);
          window_realm.heap_mut().remove_root(promise_root);
          return Err(queue_err);
        }

        return Ok(());
      }
    };

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
        let status = response.status;
        let ok = (200..300).contains(&status);
        let url = response.url.clone();
        let status_text = response.status_text.clone();

        let response_id = match with_env_state_mut(env_id, |state| {
          let id = state.alloc_id();
          state.responses.insert(id, response);
          Ok(id)
        }) {
          Ok(id) => id,
          Err(err) => {
            let message = format!("fetch failed: {err}");
            let queue_result = event_loop.queue_microtask(move |host, event_loop| {
              let window_realm = host.window_realm();
              window_realm.reset_interrupt();
              with_event_loop(event_loop, || {
                let vm = window_realm.vm_mut();
                vm.set_budget(callback_budget_from_render_deadline());
                let tick_result = vm.tick();
                let mut hooks = VmJsEventLoopHooks::<Host>::new();
                let call_result = tick_result.and_then(|_| {
                  let (vm, heap) = window_realm.vm_and_heap_mut();
                  let reject = heap.get_root(reject_root).ok_or(VmError::InvalidHandle)?;
                  let mut scope = heap.scope();
                  let type_error = create_type_error(vm, &mut scope, &mut hooks, &message)?;
                  vm.call_with_host(
                    &mut scope,
                    &mut hooks,
                    reject,
                    Value::Undefined,
                    &[type_error],
                  )?;
                  Ok(())
                });

                window_realm.heap_mut().remove_root(resolve_root);
                window_realm.heap_mut().remove_root(reject_root);
                window_realm.heap_mut().remove_root(promise_root);

                window_realm
                  .vm_mut()
                  .set_budget(Budget::unlimited(100));
                if let Some(err) = hooks.finish(window_realm.heap_mut()) {
                  return Err(err);
                }
                call_result
                  .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
                  .map(|_| ())
              })
            });

            if let Err(queue_err) = queue_result {
              let window_realm = host.window_realm();
              window_realm.heap_mut().remove_root(resolve_root);
              window_realm.heap_mut().remove_root(reject_root);
              window_realm.heap_mut().remove_root(promise_root);
              return Err(queue_err);
            }

            return Ok(());
          }
        };

        let queue_result = event_loop.queue_microtask(move |host, event_loop| {
          // Resolve the promise with a JS Response wrapper.
          let window_realm = host.window_realm();
          window_realm.reset_interrupt();
          with_event_loop(event_loop, || {
            let vm = window_realm.vm_mut();
            vm.set_budget(callback_budget_from_render_deadline());
            let tick_result = vm.tick();
            let mut hooks = VmJsEventLoopHooks::<Host>::new();

            let call_result = tick_result.and_then(|_| {
              let (vm, heap) = window_realm.vm_and_heap_mut();
              let resolve = heap.get_root(resolve_root).ok_or(VmError::InvalidHandle)?;
              let mut scope = heap.scope();

              let resp_obj = scope.alloc_object_with_prototype(Some(response_proto))?;
              scope.push_root(Value::Object(resp_obj))?;

              set_data_prop(
                &mut scope,
                resp_obj,
                ENV_ID_KEY,
                Value::Number(env_id as f64),
                false,
              )?;
              set_data_prop(
                &mut scope,
                resp_obj,
                RESPONSE_ID_KEY,
                Value::Number(response_id as f64),
                false,
              )?;
              set_data_prop(
                &mut scope,
                resp_obj,
                "status",
                Value::Number(status as f64),
                false,
              )?;
              set_data_prop(&mut scope, resp_obj, "ok", Value::Bool(ok), false)?;
              let url_s = scope.alloc_string(&url)?;
              let st_s = scope.alloc_string(&status_text)?;
              set_data_prop(&mut scope, resp_obj, "url", Value::String(url_s), false)?;
              set_data_prop(
                &mut scope,
                resp_obj,
                "statusText",
                Value::String(st_s),
                false,
              )?;

              let headers_obj = make_headers_wrapper(
                &mut scope,
                env_id,
                headers_proto,
                HEADERS_KIND_RESPONSE,
                response_id,
              )?;
              set_data_prop(
                &mut scope,
                resp_obj,
                "headers",
                Value::Object(headers_obj),
                false,
              )?;

              // Call resolve(responseObj) with host hooks so Promise jobs are enqueued onto the
              // EventLoop microtask queue.
              vm.call_with_host(
                &mut scope,
                &mut hooks,
                resolve,
                Value::Undefined,
                &[Value::Object(resp_obj)],
              )?;
              Ok(())
            });

            // Remove roots even if resolution fails.
            window_realm.heap_mut().remove_root(resolve_root);
            window_realm.heap_mut().remove_root(reject_root);
            window_realm.heap_mut().remove_root(promise_root);

            window_realm
              .vm_mut()
              .set_budget(Budget::unlimited(100));
            if let Some(err) = hooks.finish(window_realm.heap_mut()) {
              return Err(err);
            }
            call_result
              .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
              .map(|_| ())
          })
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
          return Err(queue_err);
        }
      }
      Err(err) => {
        let message = format!("fetch failed: {err}");
        let queue_result = event_loop.queue_microtask(move |host, event_loop| {
          let window_realm = host.window_realm();
          window_realm.reset_interrupt();
          with_event_loop(event_loop, || {
            let vm = window_realm.vm_mut();
            vm.set_budget(callback_budget_from_render_deadline());
            let tick_result = vm.tick();
            let mut hooks = VmJsEventLoopHooks::<Host>::new();
            let call_result = tick_result.and_then(|_| {
              let (vm, heap) = window_realm.vm_and_heap_mut();
              let reject = heap.get_root(reject_root).ok_or(VmError::InvalidHandle)?;
              let mut scope = heap.scope();
              let type_error = create_type_error(vm, &mut scope, &mut hooks, &message)?;
              vm.call_with_host(
                &mut scope,
                &mut hooks,
                reject,
                Value::Undefined,
                &[type_error],
              )?;
              Ok(())
            });

            window_realm.heap_mut().remove_root(resolve_root);
            window_realm.heap_mut().remove_root(reject_root);
            window_realm.heap_mut().remove_root(promise_root);

            window_realm
              .vm_mut()
              .set_budget(Budget::unlimited(100));
            if let Some(err) = hooks.finish(window_realm.heap_mut()) {
              return Err(err);
            }
            call_result
              .map_err(|err| vm_error_to_event_loop_error(window_realm.heap_mut(), err))
              .map(|_| ())
          })
        });

        if let Err(queue_err) = queue_result {
          let window_realm = host.window_realm();
          window_realm.heap_mut().remove_root(resolve_root);
          window_realm.heap_mut().remove_root(reject_root);
          window_realm.heap_mut().remove_root(promise_root);
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
    let err_value = create_type_error(vm, scope, host_hooks, &err.to_string())?;
    vm.call_with_host(scope, host_hooks, cap.reject, Value::Undefined, &[err_value])?;
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

  use vm_js::{HeapLimits, VmOptions};
  use vm_js::{Job, RealmId, VmHostHooks};

  struct DummyHost;

  impl WindowRealmHost for DummyHost {
    fn window_realm(&mut self) -> &mut WindowRealm {
      panic!("DummyHost.window_realm should not be called in install tests");
    }
  }

  #[test]
  fn window_fetch_bindings_drop_unregisters_env() -> Result<(), VmError> {
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
}
