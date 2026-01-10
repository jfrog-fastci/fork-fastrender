use crate::api::ConsoleMessageLevel;
use crate::dom2::{self, NodeId, NodeKind};
use crate::html::base_url_tracker::resolve_script_src_at_parse_time;
use crate::js::bindings::DomExceptionClassVmJs;
use crate::js::clock::{Clock, RealClock};
use crate::js::cookie_jar::{CookieJar, MAX_COOKIE_STRING_BYTES};
use crate::js::dom_platform::{DomInterface, DomPlatform};
use crate::js::time::{TimeBindings, WebTime};
use crate::js::document_write::{current_document_write_state_mut, DocumentWriteLimitError};
use crate::js::window_env::{
  install_window_shims_vm_js, unregister_match_media_env, MatchMediaEnvGuard, WindowEnv,
};
use crate::js::{runtime, ScriptOrchestrator, ScriptType, TaskSource, WindowHostState};
use crate::js::CurrentScriptStateHandle;
use crate::js::JsExecutionOptions;
use crate::render_control;
use crate::resource::{ensure_script_mime_sane, FetchDestination, FetchRequest, ResourceFetcher};
use crate::style::media::MediaContext;
use crate::web::events as web_events;
use base64::engine::general_purpose;
use base64::Engine as _;
use parking_lot::Mutex;
use selectors::context::QuirksMode;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use url::Url;
use vm_js::{
  GcObject, GcString, Heap, HeapLimits, HostSlots, JsRuntime as VmJsRuntime, PropertyDescriptor,
  PropertyKey, PropertyKind, Realm, RealmId, Scope, SourceText, Value, Vm, VmError, VmHost,
  VmHostHooks, VmOptions,
};

pub type ConsoleSink =
  Arc<dyn Fn(ConsoleMessageLevel, &mut vm_js::Heap, &[vm_js::Value]) + Send + Sync + 'static>;

// Compile-time guard: `vm-js` must keep exposing the borrow-splitting accessor used by FastRender
// embeddings (see `WindowRealm::new`).
#[allow(dead_code)]
const _VM_JS_RUNTIME_VM_REALM_AND_HEAP_MUT_GUARD: fn(
  &mut VmJsRuntime,
) -> (&mut Vm, &Realm, &mut Heap) = VmJsRuntime::vm_realm_and_heap_mut;

#[derive(Clone)]
pub struct WindowRealmConfig {
  pub document_url: String,
  /// Media context used for `window.devicePixelRatio`, viewport geometry, and `matchMedia()`.
  ///
  /// This should generally match the renderer's layout/styling media context for the document so
  /// JS and CSS agree on viewport/resolution queries.
  pub media: MediaContext,
  /// Optional ID of a host-owned `dom2::Document` to expose to minimal DOM shims on `window.document`.
  ///
  /// The ID refers to an entry in a thread-local registry managed by this module. This indirection
  /// keeps `vm-js` native call signatures simple: they cannot borrow the Rust host state directly,
  /// so the JS objects store an integer handle instead.
  pub dom_source_id: Option<u64>,
  /// Host-owned `Document.currentScript` state handle to expose via `document.currentScript`.
  pub current_script_state: Option<CurrentScriptStateHandle>,
  pub console_sink: Option<ConsoleSink>,
  /// Memory limits for the embedded `vm-js` heap.
  ///
  /// FastRender treats JavaScript as hostile input; keeping a hard heap limit is a foundational
  /// safety invariant even before full script execution is wired up.
  pub heap_limits: HeapLimits,
  /// Construction-time VM options for the embedded `vm-js` VM.
  ///
  /// Notably, this includes stack depth limits (`VmOptions::max_stack_depth`).
  pub vm_options: VmOptions,
  /// Host clock backing web time APIs like `Date.now()` and `performance.now()`.
  pub clock: Arc<dyn Clock>,
  /// Deterministic web time model (`performance.timeOrigin`, and the epoch offset for `Date.now()`).
  pub web_time: WebTime,
}

/// Navigation request emitted by `window.location` APIs (`href`, `assign`, `replace`).
///
/// WindowRealm itself does not perform document loading/navigation; it records a pending request and
/// interrupts the currently running script so the embedding can commit the navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationNavigationRequest {
  pub url: String,
  pub replace: bool,
}

impl WindowRealmConfig {
  pub fn new(document_url: impl Into<String>) -> Self {
    Self {
      document_url: document_url.into(),
      media: MediaContext::screen(800.0, 600.0),
      dom_source_id: None,
      current_script_state: None,
      console_sink: None,
      heap_limits: super::vm_limits::default_heap_limits(),
      vm_options: VmOptions::default(),
      clock: Arc::new(RealClock::default()),
      web_time: WebTime::default(),
    }
  }

  pub fn with_media_context(mut self, media: MediaContext) -> Self {
    self.media = media;
    self
  }

  pub fn with_dom_source_id(mut self, id: u64) -> Self {
    self.dom_source_id = Some(id);
    self
  }

  pub fn with_current_script_state(mut self, state: CurrentScriptStateHandle) -> Self {
    self.current_script_state = Some(state);
    self
  }

  pub fn with_js_execution_options(mut self, options: JsExecutionOptions) -> Self {
    self.heap_limits = super::vm_limits::heap_limits_from_js_options(&options);
    if let Some(max_stack_depth) = options.max_stack_depth {
      self.vm_options.max_stack_depth = max_stack_depth;
    }
    self
  }

  pub fn with_heap_limits(mut self, heap_limits: HeapLimits) -> Self {
    self.heap_limits = heap_limits;
    self
  }

  pub fn with_vm_options(mut self, options: VmOptions) -> Self {
    self.vm_options = options;
    self
  }

  pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
    self.clock = clock;
    self
  }

  pub fn with_web_time(mut self, web_time: WebTime) -> Self {
    self.web_time = web_time;
    self
  }
}

pub struct WindowRealm {
  runtime: Box<VmJsRuntime>,
  realm_id: RealmId,
  console_sink_id: Option<u64>,
  current_script_source_id: Option<u64>,
  match_media_env_id: Option<u64>,
  time_bindings: Option<TimeBindings>,
  interrupt_flag: Arc<AtomicBool>,
  js_execution_options: JsExecutionOptions,
}

pub(crate) struct WindowRealmUserData {
  document_url: String,
  pub(crate) base_url: Option<String>,
  pending_navigation: Option<LocationNavigationRequest>,
  cookie_fetcher: Option<Arc<dyn ResourceFetcher>>,
  cookie_jar: CookieJar,
  dom_platform: Option<DomPlatform>,
  /// Fallback `dom2::Document` used for events when the realm is not backed by a host DOM.
  ///
  /// This enables `window`/`document` (and `new EventTarget()`) event listeners in minimal realms
  /// created without [`WindowRealmConfig::dom_source_id`].
  events_dom_fallback: dom2::Document,
  /// Cached JS `window` global object for mapping `EventTargetId::Window` back into JS when the
  /// target is not a DOM-backed node/document.
  window_obj: Option<GcObject>,
  /// Cached JS `document` object for rooting event listener callbacks and mapping
  /// `EventTargetId::Document` back into JS.
  document_obj: Option<GcObject>,
}

impl std::fmt::Debug for WindowRealmUserData {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WindowRealmUserData")
      .field("document_url", &self.document_url)
      .field("base_url", &self.base_url)
      .field("has_cookie_fetcher", &self.cookie_fetcher.is_some())
      .field("cookie_jar", &self.cookie_jar)
      .field("has_dom_platform", &self.dom_platform.is_some())
      .field("has_window_obj", &self.window_obj.is_some())
      .field("has_document_obj", &self.document_obj.is_some())
      .finish()
  }
}

impl WindowRealmUserData {
  pub(crate) fn new(document_url: String) -> Self {
    Self {
      base_url: Some(document_url.clone()),
      pending_navigation: None,
      document_url,
      cookie_fetcher: None,
      cookie_jar: CookieJar::new(),
      dom_platform: None,
      events_dom_fallback: dom2::Document::new(QuirksMode::NoQuirks),
      window_obj: None,
      document_obj: None,
    }
  }
}

impl WindowRealm {
  pub fn new(config: WindowRealmConfig) -> Result<Self, VmError> {
    let mut vm_options = config.vm_options.clone();
    let heap_limits = config.heap_limits;
    let enforced_heap_max_bytes = heap_limits.max_bytes;
    let enforced_stack_depth = vm_options.max_stack_depth;

    // `WindowRealm` stores the JS execution options that drive per-run budgets. Realms created
    // directly via `WindowRealm::new` use default execution limits, but we still record the
    // enforced heap/stack bounds from the construction config for consistency/debugging.
    let mut js_execution_options = JsExecutionOptions::default();
    js_execution_options.max_vm_heap_bytes = Some(enforced_heap_max_bytes);
    js_execution_options.max_stack_depth = Some(enforced_stack_depth);

    // Window realms should be interruptible even before full script execution is wired up.
    // This is separate from the renderer-level interrupt flag: it's resettable per realm.
    let interrupt_flag = Arc::new(AtomicBool::new(false));
    vm_options.interrupt_flag = Some(Arc::clone(&interrupt_flag));
    // Also observe the render-wide interrupt flag so host cancellation interrupts JS at the next
    // `Vm::tick()`.
    vm_options.external_interrupt_flag = Some(render_control::interrupt_flag());
    let vm = Vm::new(vm_options);
    let heap = Heap::new(heap_limits);

    let mut runtime = Box::new(VmJsRuntime::new(vm, heap)?);
    runtime
      .vm
      .set_user_data(WindowRealmUserData::new(config.document_url.clone()));
    let realm_id = runtime.realm().id();

    let (vm, realm, heap) = runtime.vm_realm_and_heap_mut();

    let (console_sink_id, current_script_source_id, match_media_env_id) =
      init_window_globals(vm, heap, realm, &config)?;
    let time_bindings = match crate::js::time::install_time_bindings(
      vm,
      realm,
      heap,
      Arc::clone(&config.clock),
      config.web_time,
    ) {
      Ok(bindings) => bindings,
      Err(err) => {
        // `init_window_globals` registers host-owned resources (console sink IDs, matchMedia envs,
        // and URL binding state) that must not leak when WindowRealm initialization fails midway
        // through.
        if let Some(id) = console_sink_id {
          unregister_console_sink(id);
        }
        if let Some(id) = current_script_source_id {
          unregister_current_script_source(id);
        }
        if let Some(id) = match_media_env_id {
          unregister_match_media_env(id);
        }
        crate::js::window_url::teardown_window_url_bindings_for_realm(realm_id, heap);
        return Err(err);
      }
    };
    Ok(Self {
      runtime,
      realm_id,
      console_sink_id,
      current_script_source_id,
      match_media_env_id,
      time_bindings: Some(time_bindings),
      interrupt_flag,
      js_execution_options,
    })
  }

  pub fn new_with_js_execution_options(
    mut config: WindowRealmConfig,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self, VmError> {
    if js_execution_options.max_vm_heap_bytes.is_some() {
      // When explicitly configured, treat the heap cap as authoritative (don't apply RLIMIT scaling).
      config.heap_limits = super::vm_limits::heap_limits_from_js_options(&js_execution_options);
    }

    if let Some(max_stack_depth) = js_execution_options.max_stack_depth {
      config.vm_options.max_stack_depth = max_stack_depth;
    }
    // Also propagate per-run budgets into VM construction-time defaults. We still reset the budget
    // before each JS "turn" so the deadline is relative to the current turn and incorporates the
    // root render deadline.
    config.vm_options.default_fuel = js_execution_options.max_instruction_count;
    config.vm_options.default_deadline = js_execution_options.event_loop_run_limits.max_wall_time;
    let enforced_heap_max_bytes = config.heap_limits.max_bytes;
    let enforced_stack_depth = config.vm_options.max_stack_depth;

    let mut realm = Self::new(config)?;
    realm.js_execution_options = js_execution_options;
    // Keep stored options consistent with the realm's actual limits. Even when the caller leaves
    // these as `None`, the realm still applies a concrete heap cap + stack depth.
    realm.js_execution_options.max_vm_heap_bytes = Some(enforced_heap_max_bytes);
    realm.js_execution_options.max_stack_depth = Some(enforced_stack_depth);
    Ok(realm)
  }

  pub fn reset_interrupt(&self) {
    self.interrupt_flag.store(false, Ordering::Relaxed);
    self.runtime.vm.reset_interrupt();
  }

  pub(crate) fn vm_budget_now(&self) -> vm_js::Budget {
    self.js_execution_options.vm_js_budget_now()
  }

  pub fn heap(&self) -> &Heap {
    &self.runtime.heap
  }

  pub fn heap_mut(&mut self) -> &mut Heap {
    &mut self.runtime.heap
  }

  pub fn vm(&self) -> &Vm {
    &self.runtime.vm
  }

  pub fn vm_mut(&mut self) -> &mut Vm {
    &mut self.runtime.vm
  }

  pub fn vm_and_heap_mut(&mut self) -> (&mut Vm, &mut Heap) {
    let runtime = &mut *self.runtime;
    (&mut runtime.vm, &mut runtime.heap)
  }

  pub fn vm_realm_and_heap_mut(&mut self) -> (&mut Vm, &Realm, &mut Heap) {
    self.runtime.vm_realm_and_heap_mut()
  }

  pub fn realm(&self) -> &Realm {
    self.runtime.realm()
  }

  pub fn global_object(&self) -> GcObject {
    self.runtime.realm().global_object()
  }

  pub fn teardown(&mut self) {
    self.time_bindings.take();
    if let Some(id) = self.console_sink_id.take() {
      unregister_console_sink(id);
    }
    if let Some(id) = self.current_script_source_id.take() {
      unregister_current_script_source(id);
    }
    if let Some(id) = self.match_media_env_id.take() {
      unregister_match_media_env(id);
    }
    if let Some(data) = self.runtime.vm.user_data_mut::<WindowRealmUserData>() {
      if let Some(platform) = data.dom_platform.as_mut() {
        platform.teardown(&mut self.runtime.heap);
      }
    }
    let realm_id = self.runtime.realm().id();
    crate::js::window_url::teardown_window_url_bindings_for_realm(realm_id, &mut self.runtime.heap);
  }

  pub fn set_cookie_fetcher(&mut self, fetcher: Arc<dyn ResourceFetcher>) {
    if let Some(data) = self.runtime.vm.user_data_mut::<WindowRealmUserData>() {
      data.cookie_fetcher = Some(fetcher);
    }
  }

  /// Update the document base URL used for resolving relative URLs in JS (e.g. `fetch("rel")` and
  /// `document.baseURI`).
  pub fn set_base_url(&mut self, base_url: Option<String>) {
    if let Some(data) = self.runtime.vm.user_data_mut::<WindowRealmUserData>() {
      data.base_url = base_url;
    }
  }

  /// Returns and clears any `window.location` navigation request emitted by scripts.
  pub fn take_pending_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
    self
      .runtime
      .vm
      .user_data_mut::<WindowRealmUserData>()
      .and_then(|data| data.pending_navigation.take())
  }

  /// Execute a classic script in this window realm.
  pub fn exec_script(&mut self, source: &str) -> Result<Value, VmError> {
    self.with_vm_budget(|rt| rt.exec_script(source))
  }

  /// Execute a classic script in this window realm with an explicit embedder host context and host
  /// hook implementation.
  ///
  /// This routes Promise jobs through `VmHostHooks::host_enqueue_promise_job` instead of the
  /// VM-owned microtask queue used by [`WindowRealm::exec_script`].
  pub fn exec_script_with_host_and_hooks(
    &mut self,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    source: &str,
  ) -> Result<Value, VmError> {
    self.exec_script_source_with_host_and_hooks(
      host,
      hooks,
      Arc::new(SourceText::new("<inline>", source)),
    )
  }

  /// Execute a classic script in this window realm using a custom host hook implementation.
  ///
  /// This routes Promise jobs through `VmHostHooks::host_enqueue_promise_job` instead of the
  /// VM-owned microtask queue used by [`WindowRealm::exec_script`]. Embeddings can use this to
  /// integrate ECMAScript jobs into an HTML-shaped microtask queue.
  pub fn exec_script_with_hooks(
    &mut self,
    hooks: &mut dyn VmHostHooks,
    source: &str,
  ) -> Result<Value, VmError> {
    self.exec_script_source_with_hooks(hooks, Arc::new(SourceText::new("<inline>", source)))
  }

  pub(crate) fn exec_script_source_with_host_and_hooks(
    &mut self,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    source: Arc<SourceText>,
  ) -> Result<Value, VmError> {
    // Route Promise jobs through host hooks so embeddings can integrate with a host-owned microtask
    // queue (HTML event loop).
    self.with_vm_budget(|rt| rt.exec_script_source_with_host_and_hooks(host, hooks, source))
  }

  pub(crate) fn exec_script_source_with_hooks(
    &mut self,
    hooks: &mut dyn VmHostHooks,
    source: Arc<SourceText>,
  ) -> Result<Value, VmError> {
    let mut dummy_host = ();
    self.exec_script_source_with_host_and_hooks(&mut dummy_host, hooks, source)
  }

  /// Execute a classic script with an explicit source name for stack traces.
  pub fn exec_script_with_name(
    &mut self,
    source_name: impl Into<Arc<str>>,
    source_text: impl Into<Arc<str>>,
  ) -> Result<Value, VmError> {
    self.with_vm_budget(|rt| {
      rt.exec_script_source(Arc::new(SourceText::new(source_name, source_text)))
    })
  }

  fn with_vm_budget<T>(
    &mut self,
    f: impl FnOnce(&mut VmJsRuntime) -> Result<T, VmError>,
  ) -> Result<T, VmError> {
    let budget = self.vm_budget_now();
    let prev_budget = self.runtime.vm.swap_budget_state(budget);
    let result = (|| {
      // Ensure immediate termination when no budget remains (deadline exceeded, interrupted, etc).
      self.runtime.vm.tick()?;
      f(&mut self.runtime)
    })();
    self.runtime.vm.restore_budget_state(prev_budget);
    result
  }

  pub fn perform_microtask_checkpoint(&mut self) -> Result<(), VmError> {
    self.with_vm_budget(|rt| rt.vm.perform_microtask_checkpoint(&mut rt.heap))
  }
}

pub trait WindowRealmHost {
  /// Borrow-splits the host into:
  /// - a mutable `VmHost` context for native calls, and
  /// - a mutable `WindowRealm` for script/job execution.
  ///
  /// Implementations must ensure these borrows do not alias.
  fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm);

  fn window_realm(&mut self) -> &mut WindowRealm {
    let (_, realm) = self.vm_host_and_window_realm();
    realm
  }

  fn vm_host(&mut self) -> &mut dyn VmHost {
    let (host, _) = self.vm_host_and_window_realm();
    host
  }
}

impl Drop for WindowRealm {
  fn drop(&mut self) {
    self.teardown();
  }
}

impl vm_js::VmJobContext for WindowRealm {
  fn call(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let realm_id = self.realm_id;
    let (vm, heap) = self.vm_and_heap_mut();
    let mut scope = heap.scope();
    let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
      realm: realm_id,
      script_or_module: None,
    });
    vm.call_with_host(&mut scope, host, callee, this, args)
  }

  fn construct(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let realm_id = self.realm_id;
    let (vm, heap) = self.vm_and_heap_mut();
    let mut scope = heap.scope();
    let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
      realm: realm_id,
      script_or_module: None,
    });
    vm.construct_with_host(&mut scope, host, callee, args, new_target)
  }

  fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
    self.runtime.heap.add_root(value)
  }

  fn remove_root(&mut self, id: vm_js::RootId) {
    self.runtime.heap.remove_root(id);
  }
}

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn create_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  ctor: GcObject,
  message: &str,
) -> Result<Value, VmError> {
  let msg = scope.alloc_string(message)?;
  scope.push_root(Value::String(msg))?;
  vm.construct_with_host(
    scope,
    hooks,
    Value::Object(ctor),
    &[Value::String(msg)],
    Value::Object(ctor),
  )
}

fn throw_type_error(vm: &mut Vm, scope: &mut Scope<'_>, hooks: &mut dyn VmHostHooks, message: &str) -> VmError {
  let intr = match vm.intrinsics() {
    Some(intr) => intr,
    None => return VmError::TypeError("TypeError requires intrinsics (create a Realm first)"),
  };
  match create_error(vm, scope, hooks, intr.type_error(), message) {
    Ok(err) => VmError::Throw(err),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn illegal_dom_constructor_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(throw_type_error(vm, scope, hooks, "Illegal constructor"))
}

fn illegal_dom_constructor_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };
  illegal_dom_constructor_native(vm, scope, host, hooks, ctor, Value::Undefined, args)
}

fn storage_slots_from_callee(
  scope: &Scope<'_>,
  callee: GcObject,
) -> Result<(GcObject, GcObject), VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let this_slot = slots
    .get(STORAGE_METHOD_THIS_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);
  let data_slot = slots
    .get(STORAGE_METHOD_DATA_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);
  let Value::Object(expected_this) = this_slot else {
    return Err(VmError::InvariantViolation(
      "Storage native missing expected this slot",
    ));
  };
  let Value::Object(data_obj) = data_slot else {
    return Err(VmError::InvariantViolation(
      "Storage native missing data object slot",
    ));
  };
  Ok((expected_this, data_obj))
}

fn storage_require_this(
  scope: &Scope<'_>,
  callee: GcObject,
  this: Value,
) -> Result<GcObject, VmError> {
  let (expected_this, data_obj) = storage_slots_from_callee(scope, callee)?;
  match this {
    Value::Object(obj) if obj == expected_this => Ok(data_obj),
    _ => Err(VmError::TypeError(STORAGE_ILLEGAL_INVOCATION_ERROR)),
  }
}

fn storage_to_string(scope: &mut Scope<'_>, value: Value) -> Result<GcString, VmError> {
  let s = match value {
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };
  scope.push_root(Value::String(s))?;
  Ok(s)
}

fn storage_to_index(scope: &mut Scope<'_>, value: Value) -> Result<Option<usize>, VmError> {
  let mut n = scope.heap_mut().to_number(value)?;
  if !n.is_finite() || n.is_nan() {
    n = 0.0;
  }
  let n = n.trunc();
  if n < 0.0 {
    return Ok(None);
  }
  if n >= usize::MAX as f64 {
    return Ok(Some(usize::MAX));
  }
  Ok(Some(n as usize))
}

fn storage_length_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let data_obj = storage_require_this(scope, callee, this)?;
  let keys = scope.ordinary_own_property_keys(data_obj)?;
  let count = keys
    .iter()
    .filter(|k| matches!(k, PropertyKey::String(_)))
    .count();
  Ok(Value::Number(count as f64))
}

fn storage_get_item_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let data_obj = storage_require_this(scope, callee, this)?;
  let key_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let key_s = storage_to_string(scope, key_v)?;
  let key = PropertyKey::from_string(key_s);
  match scope
    .heap()
    .object_get_own_data_property_value(data_obj, &key)?
  {
    Some(v) => Ok(v),
    None => Ok(Value::Null),
  }
}

fn storage_set_item_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let data_obj = storage_require_this(scope, callee, this)?;
  let key_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let value_v = args.get(1).copied().unwrap_or(Value::Undefined);
  let key_s = storage_to_string(scope, key_v)?;
  let value_s = storage_to_string(scope, value_v)?;
  let key = PropertyKey::from_string(key_s);
  let value = Value::String(value_s);
  scope.define_property(
    data_obj,
    key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    },
  )?;
  Ok(Value::Undefined)
}

fn storage_remove_item_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let data_obj = storage_require_this(scope, callee, this)?;
  let key_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let key_s = storage_to_string(scope, key_v)?;
  let key = PropertyKey::from_string(key_s);
  let _ = scope.ordinary_delete(data_obj, key)?;
  Ok(Value::Undefined)
}

fn storage_clear_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let data_obj = storage_require_this(scope, callee, this)?;
  let keys = scope.ordinary_own_property_keys(data_obj)?;
  for key in keys {
    let _ = scope.ordinary_delete(data_obj, key)?;
  }
  Ok(Value::Undefined)
}

fn storage_key_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let data_obj = storage_require_this(scope, callee, this)?;
  let idx_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let Some(idx) = storage_to_index(scope, idx_v)? else {
    return Ok(Value::Null);
  };
  let keys = scope.ordinary_own_property_keys(data_obj)?;
  let mut string_keys = Vec::new();
  for key in keys {
    if let PropertyKey::String(s) = key {
      string_keys.push(s);
    }
  }
  let Some(key) = string_keys.get(idx).copied() else {
    return Ok(Value::Null);
  };
  Ok(Value::String(key))
}

fn install_storage_object(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  global: GcObject,
  global_key: PropertyKey,
  label: &str,
  length_get_call_id: vm_js::NativeFunctionId,
  get_item_call_id: vm_js::NativeFunctionId,
  set_item_call_id: vm_js::NativeFunctionId,
  remove_item_call_id: vm_js::NativeFunctionId,
  clear_call_id: vm_js::NativeFunctionId,
  key_call_id: vm_js::NativeFunctionId,
  length_key: PropertyKey,
  get_item_key: PropertyKey,
  set_item_key: PropertyKey,
  remove_item_key: PropertyKey,
  clear_key: PropertyKey,
  key_key: PropertyKey,
) -> Result<(), VmError> {
  let storage_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(storage_obj))?;
  let data_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(data_obj))?;

  let slots = [Value::Object(storage_obj), Value::Object(data_obj)];

  let make_method = |scope: &mut Scope<'_>,
                     call_id: vm_js::NativeFunctionId,
                     name: &str,
                     length: u32|
   -> Result<GcObject, VmError> {
    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function_with_slots(call_id, None, name_s, length, &slots)?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(func))?;
    Ok(func)
  };

  let get_item_func = make_method(scope, get_item_call_id, "getItem", 1)?;
  scope.define_property(
    storage_obj,
    get_item_key,
    data_desc(Value::Object(get_item_func)),
  )?;

  let set_item_func = make_method(scope, set_item_call_id, "setItem", 2)?;
  scope.define_property(
    storage_obj,
    set_item_key,
    data_desc(Value::Object(set_item_func)),
  )?;

  let remove_item_func = make_method(scope, remove_item_call_id, "removeItem", 1)?;
  scope.define_property(
    storage_obj,
    remove_item_key,
    data_desc(Value::Object(remove_item_func)),
  )?;

  let clear_func = make_method(scope, clear_call_id, "clear", 0)?;
  scope.define_property(storage_obj, clear_key, data_desc(Value::Object(clear_func)))?;

  let key_func = make_method(scope, key_call_id, "key", 1)?;
  scope.define_property(storage_obj, key_key, data_desc(Value::Object(key_func)))?;

  // Read-only `length` accessor.
  let length_get_name = scope.alloc_string(&format!("get {label}.length"))?;
  scope.push_root(Value::String(length_get_name))?;
  let length_get_func =
    scope.alloc_native_function_with_slots(length_get_call_id, None, length_get_name, 0, &slots)?;
  scope.heap_mut().object_set_prototype(
    length_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(length_get_func))?;
  scope.define_property(
    storage_obj,
    length_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(length_get_func),
        set: Value::Undefined,
      },
    },
  )?;

  // Install the storage object on the global as a read-only data property.
  scope.define_property(
    global,
    global_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(storage_obj),
        writable: false,
      },
    },
  )?;

  Ok(())
}

static NEXT_CONSOLE_SINK_ID: AtomicU64 = AtomicU64::new(1);
static CONSOLE_SINKS: OnceLock<Mutex<HashMap<u64, ConsoleSink>>> = OnceLock::new();

fn console_sinks() -> &'static Mutex<HashMap<u64, ConsoleSink>> {
  CONSOLE_SINKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_console_sink(sink: ConsoleSink) -> u64 {
  let id = NEXT_CONSOLE_SINK_ID.fetch_add(1, Ordering::Relaxed);
  console_sinks().lock().insert(id, sink);
  id
}

fn unregister_console_sink(id: u64) {
  console_sinks().lock().remove(&id);
}

struct ConsoleSinkGuard {
  id: u64,
  active: bool,
}

impl ConsoleSinkGuard {
  fn new(sink: ConsoleSink) -> Self {
    Self {
      id: register_console_sink(sink),
      active: true,
    }
  }

  fn id(&self) -> u64 {
    self.id
  }

  fn disarm(mut self) -> u64 {
    self.active = false;
    self.id
  }
}

impl Drop for ConsoleSinkGuard {
  fn drop(&mut self) {
    if self.active {
      unregister_console_sink(self.id);
    }
  }
}

const CONSOLE_LEVEL_SLOT: usize = 0;
const CONSOLE_THIS_SLOT: usize = 1;
const CONSOLE_SINK_KEY_SLOT: usize = 2;
const CONSOLE_SINK_ID_KEY: &str = "__fastrender_console_sink_id";

const LOCATION_URL_KEY: &str = "__fastrender_location_url";
const LOCATION_ACCESSOR_LOCATION_OBJ_SLOT: usize = 0;
const STORAGE_METHOD_THIS_SLOT: usize = 0;
const STORAGE_METHOD_DATA_SLOT: usize = 1;
const STORAGE_ILLEGAL_INVOCATION_ERROR: &str = "Illegal invocation";
const EVENT_TARGET_DEFAULT_THIS_SLOT: usize = 0;
const EVENT_TARGET_CONTEXT_GLOBAL_SLOT: usize = 1;
const EVENT_TARGET_BRAND_KEY: &str = "__fastrender_event_target";
const CURRENT_SCRIPT_SOURCE_ID_KEY: &str = "__fastrender_current_script_source_id";
const NODE_ID_KEY: &str = "__fastrender_node_id";
const DOM_SOURCE_ID_KEY: &str = "__fastrender_dom_source_id";
const DOM_STRING_MAP_HOST_KIND: u64 = 4;
const WRAPPER_DOCUMENT_KEY: &str = "__fastrender_wrapper_document";
const DOCUMENT_WINDOW_KEY: &str = "__fastrender_document_window";
const EVENT_PROTOTYPE_KEY: &str = "__fastrender_event_prototype";
const CUSTOM_EVENT_PROTOTYPE_KEY: &str = "__fastrender_custom_event_prototype";
const EVENT_ID_KEY: &str = "__fastrender_event_id";
const EVENT_LISTENER_ROOTS_KEY: &str = "__fastrender_event_listener_roots";
const EVENT_TARGET_ADD_EVENT_LISTENER_KEY: &str = "__fastrender_event_target_add_event_listener";
const EVENT_TARGET_REMOVE_EVENT_LISTENER_KEY: &str =
  "__fastrender_event_target_remove_event_listener";
const EVENT_TARGET_DISPATCH_EVENT_KEY: &str = "__fastrender_event_target_dispatch_event";
const ELEMENT_CLASS_NAME_GET_KEY: &str = "__fastrender_element_class_name_get";
const ELEMENT_CLASS_NAME_SET_KEY: &str = "__fastrender_element_class_name_set";
const ELEMENT_CLASS_LIST_ADD_KEY: &str = "__fastrender_element_class_list_add";
const ELEMENT_CLASS_LIST_REMOVE_KEY: &str = "__fastrender_element_class_list_remove";
const ELEMENT_CLASS_LIST_TOGGLE_KEY: &str = "__fastrender_element_class_list_toggle";
const ELEMENT_CLASS_LIST_CONTAINS_KEY: &str = "__fastrender_element_class_list_contains";
const ELEMENT_CLASS_LIST_REPLACE_KEY: &str = "__fastrender_element_class_list_replace";
const ELEMENT_ID_GET_KEY: &str = "__fastrender_element_id_get";
const ELEMENT_ID_SET_KEY: &str = "__fastrender_element_id_set";
const ELEMENT_SRC_GET_KEY: &str = "__fastrender_element_src_get";
const ELEMENT_SRC_SET_KEY: &str = "__fastrender_element_src_set";
const ELEMENT_SRCSET_GET_KEY: &str = "__fastrender_element_srcset_get";
const ELEMENT_SRCSET_SET_KEY: &str = "__fastrender_element_srcset_set";
const ELEMENT_SIZES_GET_KEY: &str = "__fastrender_element_sizes_get";
const ELEMENT_SIZES_SET_KEY: &str = "__fastrender_element_sizes_set";
const ELEMENT_HREF_GET_KEY: &str = "__fastrender_element_href_get";
const ELEMENT_HREF_SET_KEY: &str = "__fastrender_element_href_set";
const ELEMENT_REL_GET_KEY: &str = "__fastrender_element_rel_get";
const ELEMENT_REL_SET_KEY: &str = "__fastrender_element_rel_set";
const ELEMENT_TYPE_GET_KEY: &str = "__fastrender_element_type_get";
const ELEMENT_TYPE_SET_KEY: &str = "__fastrender_element_type_set";
const ELEMENT_CHARSET_GET_KEY: &str = "__fastrender_element_charset_get";
const ELEMENT_CHARSET_SET_KEY: &str = "__fastrender_element_charset_set";
const ELEMENT_CROSS_ORIGIN_GET_KEY: &str = "__fastrender_element_cross_origin_get";
const ELEMENT_CROSS_ORIGIN_SET_KEY: &str = "__fastrender_element_cross_origin_set";
const ELEMENT_ASYNC_GET_KEY: &str = "__fastrender_element_async_get";
const ELEMENT_ASYNC_SET_KEY: &str = "__fastrender_element_async_set";
const ELEMENT_DEFER_GET_KEY: &str = "__fastrender_element_defer_get";
const ELEMENT_DEFER_SET_KEY: &str = "__fastrender_element_defer_set";
const ELEMENT_HEIGHT_GET_KEY: &str = "__fastrender_element_height_get";
const ELEMENT_HEIGHT_SET_KEY: &str = "__fastrender_element_height_set";
const ELEMENT_WIDTH_GET_KEY: &str = "__fastrender_element_width_get";
const ELEMENT_WIDTH_SET_KEY: &str = "__fastrender_element_width_set";
const STYLE_GET_PROPERTY_VALUE_KEY: &str = "__fastrender_style_get_property_value";
const STYLE_SET_PROPERTY_KEY: &str = "__fastrender_style_set_property";
const STYLE_REMOVE_PROPERTY_KEY: &str = "__fastrender_style_remove_property";
const STYLE_CSS_TEXT_GET_KEY: &str = "__fastrender_style_css_text_get";
const STYLE_CSS_TEXT_SET_KEY: &str = "__fastrender_style_css_text_set";
const STYLE_DISPLAY_GET_KEY: &str = "__fastrender_style_display_get";
const STYLE_DISPLAY_SET_KEY: &str = "__fastrender_style_display_set";
const STYLE_CURSOR_GET_KEY: &str = "__fastrender_style_cursor_get";
const STYLE_CURSOR_SET_KEY: &str = "__fastrender_style_cursor_set";
const STYLE_HEIGHT_GET_KEY: &str = "__fastrender_style_height_get";
const STYLE_HEIGHT_SET_KEY: &str = "__fastrender_style_height_set";
const STYLE_WIDTH_GET_KEY: &str = "__fastrender_style_width_get";
const STYLE_WIDTH_SET_KEY: &str = "__fastrender_style_width_set";
const NODE_APPEND_CHILD_KEY: &str = "__fastrender_node_append_child";
const NODE_INSERT_BEFORE_KEY: &str = "__fastrender_node_insert_before";
const NODE_REMOVE_CHILD_KEY: &str = "__fastrender_node_remove_child";
const NODE_REPLACE_CHILD_KEY: &str = "__fastrender_node_replace_child";
const NODE_CLONE_NODE_KEY: &str = "__fastrender_node_clone_node";
const NODE_PARENT_NODE_GET_KEY: &str = "__fastrender_node_parent_node_get";
const NODE_FIRST_CHILD_GET_KEY: &str = "__fastrender_node_first_child_get";
const NODE_PREVIOUS_SIBLING_GET_KEY: &str = "__fastrender_node_previous_sibling_get";
const NODE_NEXT_SIBLING_GET_KEY: &str = "__fastrender_node_next_sibling_get";
const NODE_REMOVE_KEY: &str = "__fastrender_node_remove";
const NODE_TEXT_CONTENT_GET_KEY: &str = "__fastrender_node_text_content_get";
const NODE_TEXT_CONTENT_SET_KEY: &str = "__fastrender_node_text_content_set";
const NODE_CHILD_NODES_KEY: &str = "__fastrender_node_child_nodes";
const ELEMENT_GET_ATTRIBUTE_KEY: &str = "__fastrender_element_get_attribute";
const ELEMENT_SET_ATTRIBUTE_KEY: &str = "__fastrender_element_set_attribute";
const ELEMENT_REMOVE_ATTRIBUTE_KEY: &str = "__fastrender_element_remove_attribute";
const ELEMENT_INNER_HTML_GET_KEY: &str = "__fastrender_element_inner_html_get";
const ELEMENT_INNER_HTML_SET_KEY: &str = "__fastrender_element_inner_html_set";
const ELEMENT_OUTER_HTML_GET_KEY: &str = "__fastrender_element_outer_html_get";
const ELEMENT_OUTER_HTML_SET_KEY: &str = "__fastrender_element_outer_html_set";
const ELEMENT_INSERT_ADJACENT_HTML_KEY: &str = "__fastrender_element_insert_adjacent_html";
const ELEMENT_INSERT_ADJACENT_ELEMENT_KEY: &str = "__fastrender_element_insert_adjacent_element";
const ELEMENT_INSERT_ADJACENT_TEXT_KEY: &str = "__fastrender_element_insert_adjacent_text";
const ELEMENT_QUERY_SELECTOR_KEY: &str = "__fastrender_element_query_selector";
const ELEMENT_QUERY_SELECTOR_ALL_KEY: &str = "__fastrender_element_query_selector_all";
const ELEMENT_MATCHES_KEY: &str = "__fastrender_element_matches";
const ELEMENT_CLOSEST_KEY: &str = "__fastrender_element_closest";

static NEXT_CURRENT_SCRIPT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);

static NEXT_DOM_SOURCE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_ACTIVE_EVENT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
  static CURRENT_SCRIPT_SOURCES: RefCell<HashMap<u64, CurrentScriptStateHandle>> =
    RefCell::new(HashMap::new());
  static DOM_SOURCES: RefCell<HashMap<u64, NonNull<dom2::Document>>> =
    RefCell::new(HashMap::new());
  static ACTIVE_EVENTS: RefCell<HashMap<u64, NonNull<web_events::Event>>> =
    RefCell::new(HashMap::new());
}

fn register_current_script_source(state: CurrentScriptStateHandle) -> u64 {
  let id = NEXT_CURRENT_SCRIPT_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
  CURRENT_SCRIPT_SOURCES.with(|sources| sources.borrow_mut().insert(id, state));
  id
}

fn unregister_current_script_source(id: u64) {
  CURRENT_SCRIPT_SOURCES.with(|sources| {
    sources.borrow_mut().remove(&id);
  });
}

pub(crate) fn register_dom_source(dom: NonNull<dom2::Document>) -> u64 {
  let id = NEXT_DOM_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
  DOM_SOURCES.with(|sources| sources.borrow_mut().insert(id, dom));
  id
}

pub(crate) fn unregister_dom_source(id: u64) {
  DOM_SOURCES.with(|sources| {
    sources.borrow_mut().remove(&id);
  });
}

fn dom_for_source(id: u64) -> Option<NonNull<dom2::Document>> {
  DOM_SOURCES.with(|sources| sources.borrow().get(&id).copied())
}

#[cfg(test)]
pub(crate) fn is_dom_source_registered(id: u64) -> bool {
  DOM_SOURCES.with(|sources| sources.borrow().contains_key(&id))
}

fn event_active_event_id(scope: &mut Scope<'_>, event_obj: GcObject) -> Result<Option<u64>, VmError> {
  let key = alloc_key(scope, EVENT_ID_KEY)?;
  Ok(match scope.heap().object_get_own_data_property_value(event_obj, &key)? {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => Some(n as u64),
    _ => None,
  })
}

fn with_active_dom_event<R>(
  event_id: u64,
  f: impl FnOnce(&mut web_events::Event) -> R,
) -> Option<R> {
  let ptr = ACTIVE_EVENTS.with(|events| events.borrow().get(&event_id).copied())?;
  // Safety: the pointer is installed by the dispatch invoker for the duration of a listener call.
  Some(unsafe { f(&mut *ptr.as_ptr()) })
}

pub(crate) fn dataset_exotic_get(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
) -> Result<Option<Value>, VmError> {
  // `host_exotic_get` is called for *all* objects, including VM-internal object kinds like
  // Promises/TypedArrays. `Heap::object_host_slots` only supports ordinary objects/functions; for
  // other object kinds, treat this as "no host slots" rather than failing the property access.
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle) if scope.heap().is_valid_object(obj) => None,
    Err(err) => return Err(err),
  };
  let Some(slots) = slots else {
    return Ok(None);
  };
  if slots.b != DOM_STRING_MAP_HOST_KIND {
    return Ok(None);
  }

  let PropertyKey::String(prop_s) = key else {
    return Ok(None);
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(None),
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(None);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let node_index = match usize::try_from(slots.a) {
    Ok(v) => v,
    Err(_) => return Ok(None),
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(None),
  };

  let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();
  let Some(value) = dom.dataset_get(node_id, &prop) else {
    return Ok(None);
  };
  Ok(Some(Value::String(scope.alloc_string(value)?)))
}

pub(crate) fn dataset_exotic_set(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
  value: Value,
) -> Result<Option<bool>, VmError> {
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle) if scope.heap().is_valid_object(obj) => None,
    Err(err) => return Err(err),
  };
  let Some(slots) = slots else {
    return Ok(None);
  };
  if slots.b != DOM_STRING_MAP_HOST_KIND {
    return Ok(None);
  }

  let PropertyKey::String(prop_s) = key else {
    return Ok(None);
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(None),
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Ok(None);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };

  let node_index = match usize::try_from(slots.a) {
    Ok(v) => v,
    Err(_) => return Ok(None),
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(None),
  };

  let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();
  let value_value = scope.heap_mut().to_string(value)?;
  let value = scope
    .heap()
    .get_string(value_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  dom
    .dataset_set(node_id, &prop, &value)
    .map_err(|_| VmError::TypeError("failed to set dataset property"))?;

  Ok(Some(true))
}

pub(crate) fn dataset_exotic_delete(
  scope: &mut Scope<'_>,
  obj: GcObject,
  key: PropertyKey,
) -> Result<Option<bool>, VmError> {
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle) if scope.heap().is_valid_object(obj) => None,
    Err(err) => return Err(err),
  };
  let Some(slots) = slots else {
    return Ok(None);
  };
  if slots.b != DOM_STRING_MAP_HOST_KIND {
    return Ok(None);
  }

  let PropertyKey::String(prop_s) = key else {
    return Ok(None);
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(None),
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Ok(None);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };

  let node_index = match usize::try_from(slots.a) {
    Ok(v) => v,
    Err(_) => return Ok(None),
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(None),
  };

  let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();

  dom
    .dataset_delete(node_id, &prop)
    .map_err(|_| VmError::TypeError("failed to delete dataset property"))?;

  Ok(Some(true))
}

fn current_script_for_source(id: u64) -> Option<NodeId> {
  CURRENT_SCRIPT_SOURCES.with(|sources| {
    let sources = sources.borrow();
    let state = sources.get(&id)?;
    let current = {
      let state = state.borrow();
      state.current_script
    };
    current
  })
}

struct CurrentScriptSourceGuard {
  id: u64,
  active: bool,
}

impl CurrentScriptSourceGuard {
  fn new(state: CurrentScriptStateHandle) -> Self {
    Self {
      id: register_current_script_source(state),
      active: true,
    }
  }

  fn id(&self) -> u64 {
    self.id
  }

  fn disarm(mut self) -> u64 {
    self.active = false;
    self.id
  }
}

impl Drop for CurrentScriptSourceGuard {
  fn drop(&mut self) {
    if self.active {
      unregister_current_script_source(self.id);
    }
  }
}

const MAX_BASE64_INPUT_LEN: usize = 32 * 1024 * 1024;
const MAX_BASE64_OUTPUT_LEN: usize = 32 * 1024 * 1024;

fn console_call_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let level = match slots.get(CONSOLE_LEVEL_SLOT).copied().unwrap_or(Value::Undefined) {
    Value::Number(n) => match n as u8 {
      1 => ConsoleMessageLevel::Info,
      2 => ConsoleMessageLevel::Warn,
      3 => ConsoleMessageLevel::Error,
      4 => ConsoleMessageLevel::Debug,
      _ => ConsoleMessageLevel::Log,
    },
    _ => ConsoleMessageLevel::Log,
  };
  let console_obj = match this {
    Value::Object(obj) => obj,
    _ => match slots.get(CONSOLE_THIS_SLOT).copied().unwrap_or(Value::Undefined) {
      Value::Object(obj) => obj,
      _ => return Ok(Value::Undefined),
    },
  };
  let sink_key_s = match slots
    .get(CONSOLE_SINK_KEY_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => return Ok(Value::Undefined),
  };

  let sink_id_key = PropertyKey::from_string(sink_key_s);
  let id = match scope
    .heap()
    .object_get_own_data_property_value(console_obj, &sink_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Undefined),
  };

  let sink = console_sinks().lock().get(&id).cloned();
  if let Some(sink) = sink {
    sink(level, scope.heap_mut(), args);
  }

  Ok(Value::Undefined)
}

const REPORT_ERROR_SINK_ID_SLOT: usize = 0;

fn window_report_error_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // `reportError` must not throw, so avoid `?` and ignore any formatting failures.
  let sink = (|| {
    let slots = scope.heap().get_function_native_slots(callee).ok()?;
    let id = match slots
      .get(REPORT_ERROR_SINK_ID_SLOT)
      .copied()
      .unwrap_or(Value::Undefined)
    {
      Value::Number(n) => n as u64,
      _ => return None,
    };
    console_sinks().lock().get(&id).cloned()
  })();

  if let Some(sink) = sink {
    sink(ConsoleMessageLevel::Error, scope.heap_mut(), args);
  }

  Ok(Value::Undefined)
}

fn window_btoa_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = match input {
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };
  let code_units = scope.heap().get_string(s)?.as_code_units();

  if code_units.len() > MAX_BASE64_INPUT_LEN {
    return Err(VmError::Throw(make_dom_exception(
      scope,
      "InvalidCharacterError",
      "The string to be encoded is too large.",
    )?));
  }

  let mut bytes: Vec<u8> = Vec::new();
  bytes
    .try_reserve_exact(code_units.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for &u in code_units {
    if u > 0xFF {
      return Err(VmError::Throw(make_dom_exception(
        scope,
        "InvalidCharacterError",
        "The string to be encoded contains characters outside of the Latin1 range.",
      )?));
    }
    bytes.push(u as u8);
  }

  let expected_len = (bytes.len().saturating_add(2) / 3).saturating_mul(4);
  if expected_len > MAX_BASE64_OUTPUT_LEN {
    return Err(VmError::Throw(make_dom_exception(
      scope,
      "InvalidCharacterError",
      "The string to be encoded is too large.",
    )?));
  }

  // HTML's "forgiving-base64 encode" uses the standard alphabet with padding and no line breaks.
  let encoded = general_purpose::STANDARD.encode(bytes);
  if encoded.len() > MAX_BASE64_OUTPUT_LEN {
    return Err(VmError::Throw(make_dom_exception(
      scope,
      "InvalidCharacterError",
      "The string to be encoded is too large.",
    )?));
  }
  let out = scope.alloc_string(&encoded)?;
  Ok(Value::String(out))
}

fn window_atob_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let input = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = match input {
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };
  let code_units = scope.heap().get_string(s)?.as_code_units();

  if code_units.len() > MAX_BASE64_INPUT_LEN {
    return Err(VmError::Throw(make_dom_exception(
      scope,
      "InvalidCharacterError",
      "The string to be decoded is too large.",
    )?));
  }

  // HTML's forgiving-base64 decode algorithm.
  let mut stripped: Vec<u8> = Vec::new();
  stripped
    .try_reserve_exact(code_units.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for &unit in code_units {
    if is_html_ascii_whitespace_unit(unit) {
      continue;
    }
    if unit > 0xFF {
      return Err(VmError::Throw(make_dom_exception(
        scope,
        "InvalidCharacterError",
        "The string to be decoded is not correctly encoded.",
      )?));
    }
    stripped.push(unit as u8);
  }

  // If length mod 4 is 0, remove up to two '=' from the end.
  if stripped.len() % 4 == 0 {
    let mut removed = 0usize;
    while removed < 2 && stripped.last() == Some(&b'=') {
      stripped.pop();
      removed += 1;
    }
  }

  // If length mod 4 is 1, fail.
  if stripped.len() % 4 == 1 {
    return Err(VmError::Throw(make_dom_exception(
      scope,
      "InvalidCharacterError",
      "The string to be decoded is not correctly encoded.",
    )?));
  }

  // If it contains a non-base64 character, fail.
  if stripped
    .iter()
    .copied()
    .any(|b| !is_base64_alphabet_byte(b))
  {
    return Err(VmError::Throw(make_dom_exception(
      scope,
      "InvalidCharacterError",
      "The string to be decoded is not correctly encoded.",
    )?));
  }

  // Pad with '=' until length mod 4 is 0.
  while stripped.len() % 4 != 0 {
    stripped.push(b'=');
  }

  let decoded = match general_purpose::STANDARD.decode(&stripped) {
    Ok(decoded) => decoded,
    Err(_) => {
      return Err(VmError::Throw(make_dom_exception(
        scope,
        "InvalidCharacterError",
        "The string to be decoded is not correctly encoded.",
      )?));
    }
  };

  if decoded.len() > MAX_BASE64_OUTPUT_LEN {
    return Err(VmError::Throw(make_dom_exception(
      scope,
      "InvalidCharacterError",
      "The string to be decoded is too large.",
    )?));
  }

  let mut units: Vec<u16> = Vec::new();
  units
    .try_reserve_exact(decoded.len())
    .map_err(|_| VmError::OutOfMemory)?;
  units.extend(decoded.iter().map(|&b| b as u16));
  let out = scope.alloc_string_from_u16_vec(units)?;
  Ok(Value::String(out))
}

fn make_dom_exception(scope: &mut Scope<'_>, name: &str, message: &str) -> Result<Value, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  let name_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(name_s))?;
  let message_s = scope.alloc_string(message)?;
  scope.push_root(Value::String(message_s))?;

  let name_key = alloc_key(scope, "name")?;
  let message_key = alloc_key(scope, "message")?;

  scope.define_property(
    obj,
    name_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(name_s),
        writable: false,
      },
    },
  )?;
  scope.define_property(
    obj,
    message_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(message_s),
        writable: false,
      },
    },
  )?;

  Ok(Value::Object(obj))
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

fn is_secure_context_for_document_url(url: &str) -> bool {
  let Ok(url) = Url::parse(url) else {
    return false;
  };
  match url.scheme() {
    "https" => true,
    "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1")),
    _ => false,
  }
}

fn is_base64_alphabet_byte(b: u8) -> bool {
  matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/')
}

fn is_html_ascii_whitespace_unit(unit: u16) -> bool {
  matches!(unit, 0x09 | 0x0A | 0x0C | 0x0D | 0x20)
}

fn location_href_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let key = alloc_key(scope, LOCATION_URL_KEY)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(location_obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn window_location_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  Ok(
    slots
      .get(LOCATION_ACCESSOR_LOCATION_OBJ_SLOT)
      .copied()
      .unwrap_or(Value::Undefined),
  )
}

fn request_location_navigation(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  location_obj: Option<GcObject>,
  url_value: Value,
  replace: bool,
) -> Result<Value, VmError> {
  let base_url = {
    let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
      return Err(VmError::InvariantViolation(
        "window realm missing user data",
      ));
    };
    data.base_url.clone()
  };

  let url_value = match url_value {
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };
  scope.push_root(Value::String(url_value))?;
  let url_input = scope.heap().get_string(url_value)?.to_utf8_lossy();

  let resolved = crate::js::url_resolve::resolve_url(&url_input, base_url.as_deref())
    .map_err(|err| throw_type_error(vm, scope, hooks, &err.to_string()))?;

  let parsed =
    Url::parse(&resolved).map_err(|err| throw_type_error(vm, scope, hooks, &err.to_string()))?;
  match parsed.scheme() {
    "http" | "https" | "file" | "data" | "about" => {}
    other => {
      return Err(throw_type_error(
        vm,
        scope,
        hooks,
        &format!("Navigation to {other}: URLs is not supported"),
      ));
    }
  }

  if let Some(location_obj) = location_obj {
    // Keep the resolved URL on the location object so `location.href` immediately reflects the new
    // target.
    let key = alloc_key(scope, LOCATION_URL_KEY)?;
    let resolved_s = scope.alloc_string(&resolved)?;
    scope.push_root(Value::String(resolved_s))?;
    scope.define_property(location_obj, key, data_desc(Value::String(resolved_s)))?;
  }

  {
    let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
      return Err(VmError::InvariantViolation(
        "window realm missing user data",
      ));
    };
    data.pending_navigation = Some(LocationNavigationRequest { url: resolved, replace });
  }

  // Abort the currently running script so the embedding can commit navigation synchronously (e.g.
  // cancel streaming HTML parsing and replace the document).
  vm.interrupt_handle().interrupt();
  // `InterruptHandle` is only observed at `Vm::tick()` boundaries; force a tick now so the
  // termination propagates out of this native call immediately.
  vm.tick()?;
  Ok(Value::Undefined)
}

fn window_location_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let location_obj = slots.get(LOCATION_ACCESSOR_LOCATION_OBJ_SLOT).copied().and_then(|value| {
    let Value::Object(obj) = value else {
      return None;
    };
    Some(obj)
  });
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  request_location_navigation(vm, scope, hooks, location_obj, url_value, false)
}

fn location_href_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  request_location_navigation(vm, scope, hooks, Some(location_obj), url_value, false)
}

fn location_assign_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  request_location_navigation(vm, scope, hooks, Some(location_obj), url_value, false)
}

fn location_replace_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  request_location_navigation(vm, scope, hooks, Some(location_obj), url_value, true)
}

fn location_set_unimplemented_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "Navigation via location is not implemented yet",
  ))
}

fn parse_location_url(
  scope: &mut Scope<'_>,
  location_obj: GcObject,
) -> Result<Option<Url>, VmError> {
  let key = alloc_key(scope, LOCATION_URL_KEY)?;
  let value = scope
    .heap()
    .object_get_own_data_property_value(location_obj, &key)?
    .unwrap_or(Value::Undefined);
  let Value::String(s) = value else {
    return Ok(None);
  };
  let href = scope.heap().get_string(s)?.to_utf8_lossy();
  Ok(Url::parse(&href).ok())
}

fn location_protocol_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let Some(url) = parse_location_url(scope, location_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let protocol = format!("{}:", url.scheme());
  Ok(Value::String(scope.alloc_string(&protocol)?))
}

fn location_host_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let Some(url) = parse_location_url(scope, location_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let host = url.host_str().unwrap_or("");
  let port = url.port();
  let mut out = String::new();
  out.push_str(host);
  if let Some(port) = port {
    use std::fmt::Write as _;
    out.push(':');
    let _ = write!(&mut out, "{port}");
  }
  Ok(Value::String(scope.alloc_string(&out)?))
}

fn location_hostname_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let Some(url) = parse_location_url(scope, location_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  Ok(Value::String(
    scope.alloc_string(url.host_str().unwrap_or(""))?,
  ))
}

fn location_port_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let Some(url) = parse_location_url(scope, location_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let Some(port) = url.port() else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  Ok(Value::String(scope.alloc_string(&port.to_string())?))
}

fn location_pathname_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let Some(url) = parse_location_url(scope, location_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  Ok(Value::String(scope.alloc_string(url.path())?))
}

fn location_search_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let Some(url) = parse_location_url(scope, location_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let Some(query) = url.query() else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let search = format!("?{query}");
  Ok(Value::String(scope.alloc_string(&search)?))
}

fn location_hash_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let Some(url) = parse_location_url(scope, location_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let Some(fragment) = url.fragment() else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let hash = format!("#{fragment}");
  Ok(Value::String(scope.alloc_string(&hash)?))
}

fn decimal_str_for_usize(mut value: usize, buf: &mut [u8; 20]) -> &str {
  let mut i = buf.len();
  if value == 0 {
    i -= 1;
    buf[i] = b'0';
  } else {
    while value > 0 {
      i -= 1;
      buf[i] = b'0' + (value % 10) as u8;
      value /= 10;
    }
  }
  // SAFETY: digits are always valid UTF-8.
  unsafe { std::str::from_utf8_unchecked(&buf[i..]) }
}

fn dom_platform_mut(vm: &mut Vm) -> Option<&mut DomPlatform> {
  vm
    .user_data_mut::<WindowRealmUserData>()
    .and_then(|data| data.dom_platform.as_mut())
}
fn get_or_create_node_wrapper(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  document_obj: GcObject,
  node_id: NodeId,
) -> Result<Value, VmError> {
  if node_id.index() == 0 {
    // `dom2`'s document node is always index 0; the canonical wrapper is `window.document`.
    return Ok(Value::Object(document_obj));
  }

  let wrapper = if let Some(platform) = dom_platform_mut(vm) {
    if let Some(existing) = platform.get_existing_wrapper(scope.heap(), node_id) {
      return Ok(Value::Object(existing));
    }

    let mut primary = DomInterface::Node;
    if let Some(dom_ptr) = dom_for_source(platform.dom_source_id()) {
      // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for
      // the lifetime of the associated host document.
      let dom = unsafe { dom_ptr.as_ref() };
      primary = DomInterface::primary_for_node_kind(&dom.node(node_id).kind);
    }

    platform.get_or_create_wrapper(scope, node_id, primary)?
  } else {
    scope.alloc_object()?
  };
  scope.push_root(Value::Object(wrapper))?;

  let dom_source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let dom_source_id_value = scope
    .heap()
    .object_get_own_data_property_value(document_obj, &dom_source_id_key)?
    .unwrap_or(Value::Undefined);

  let element_query_selector = {
    let key = alloc_key(scope, ELEMENT_QUERY_SELECTOR_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let element_query_selector_all = {
    let key = alloc_key(scope, ELEMENT_QUERY_SELECTOR_ALL_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let element_matches = {
    let key = alloc_key(scope, ELEMENT_MATCHES_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let element_closest = {
    let key = alloc_key(scope, ELEMENT_CLOSEST_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };

  let class_name_get = {
    let key = alloc_key(scope, ELEMENT_CLASS_NAME_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let class_name_set = {
    let key = alloc_key(scope, ELEMENT_CLASS_NAME_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let class_list_add = {
    let key = alloc_key(scope, ELEMENT_CLASS_LIST_ADD_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let class_list_remove = {
    let key = alloc_key(scope, ELEMENT_CLASS_LIST_REMOVE_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let class_list_toggle = {
    let key = alloc_key(scope, ELEMENT_CLASS_LIST_TOGGLE_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let class_list_contains = {
    let key = alloc_key(scope, ELEMENT_CLASS_LIST_CONTAINS_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let class_list_replace = {
    let key = alloc_key(scope, ELEMENT_CLASS_LIST_REPLACE_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let id_get = {
    let key = alloc_key(scope, ELEMENT_ID_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let id_set = {
    let key = alloc_key(scope, ELEMENT_ID_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let src_get = {
    let key = alloc_key(scope, ELEMENT_SRC_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let src_set = {
    let key = alloc_key(scope, ELEMENT_SRC_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let srcset_get = {
    let key = alloc_key(scope, ELEMENT_SRCSET_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let srcset_set = {
    let key = alloc_key(scope, ELEMENT_SRCSET_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let sizes_get = {
    let key = alloc_key(scope, ELEMENT_SIZES_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let sizes_set = {
    let key = alloc_key(scope, ELEMENT_SIZES_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let href_get = {
    let key = alloc_key(scope, ELEMENT_HREF_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let href_set = {
    let key = alloc_key(scope, ELEMENT_HREF_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let rel_get = {
    let key = alloc_key(scope, ELEMENT_REL_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let rel_set = {
    let key = alloc_key(scope, ELEMENT_REL_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let type_get = {
    let key = alloc_key(scope, ELEMENT_TYPE_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let type_set = {
    let key = alloc_key(scope, ELEMENT_TYPE_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let charset_get = {
    let key = alloc_key(scope, ELEMENT_CHARSET_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let charset_set = {
    let key = alloc_key(scope, ELEMENT_CHARSET_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let cross_origin_get = {
    let key = alloc_key(scope, ELEMENT_CROSS_ORIGIN_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let cross_origin_set = {
    let key = alloc_key(scope, ELEMENT_CROSS_ORIGIN_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let async_get = {
    let key = alloc_key(scope, ELEMENT_ASYNC_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let async_set = {
    let key = alloc_key(scope, ELEMENT_ASYNC_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let defer_get = {
    let key = alloc_key(scope, ELEMENT_DEFER_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let defer_set = {
    let key = alloc_key(scope, ELEMENT_DEFER_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let height_get = {
    let key = alloc_key(scope, ELEMENT_HEIGHT_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let height_set = {
    let key = alloc_key(scope, ELEMENT_HEIGHT_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let width_get = {
    let key = alloc_key(scope, ELEMENT_WIDTH_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let width_set = {
    let key = alloc_key(scope, ELEMENT_WIDTH_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let append_child = {
    let key = alloc_key(scope, NODE_APPEND_CHILD_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let insert_before = {
    let key = alloc_key(scope, NODE_INSERT_BEFORE_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let remove_child = {
    let key = alloc_key(scope, NODE_REMOVE_CHILD_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let replace_child = {
    let key = alloc_key(scope, NODE_REPLACE_CHILD_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let clone_node = {
    let key = alloc_key(scope, NODE_CLONE_NODE_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let parent_node_get = {
    let key = alloc_key(scope, NODE_PARENT_NODE_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let first_child_get = {
    let key = alloc_key(scope, NODE_FIRST_CHILD_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let previous_sibling_get = {
    let key = alloc_key(scope, NODE_PREVIOUS_SIBLING_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let next_sibling_get = {
    let key = alloc_key(scope, NODE_NEXT_SIBLING_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let node_remove = {
    let key = alloc_key(scope, NODE_REMOVE_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let text_content_get = {
    let key = alloc_key(scope, NODE_TEXT_CONTENT_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let text_content_set = {
    let key = alloc_key(scope, NODE_TEXT_CONTENT_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let add_event_listener = {
    let key = alloc_key(scope, EVENT_TARGET_ADD_EVENT_LISTENER_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let remove_event_listener = {
    let key = alloc_key(scope, EVENT_TARGET_REMOVE_EVENT_LISTENER_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let dispatch_event = {
    let key = alloc_key(scope, EVENT_TARGET_DISPATCH_EVENT_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let get_attribute = {
    let key = alloc_key(scope, ELEMENT_GET_ATTRIBUTE_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let set_attribute = {
    let key = alloc_key(scope, ELEMENT_SET_ATTRIBUTE_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let remove_attribute = {
    let key = alloc_key(scope, ELEMENT_REMOVE_ATTRIBUTE_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let inner_html_get = {
    let key = alloc_key(scope, ELEMENT_INNER_HTML_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let inner_html_set = {
    let key = alloc_key(scope, ELEMENT_INNER_HTML_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let outer_html_get = {
    let key = alloc_key(scope, ELEMENT_OUTER_HTML_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let outer_html_set = {
    let key = alloc_key(scope, ELEMENT_OUTER_HTML_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let insert_adjacent_html = {
    let key = alloc_key(scope, ELEMENT_INSERT_ADJACENT_HTML_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let insert_adjacent_element = {
    let key = alloc_key(scope, ELEMENT_INSERT_ADJACENT_ELEMENT_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let insert_adjacent_text = {
    let key = alloc_key(scope, ELEMENT_INSERT_ADJACENT_TEXT_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_get_property_value = {
    let key = alloc_key(scope, STYLE_GET_PROPERTY_VALUE_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_set_property = {
    let key = alloc_key(scope, STYLE_SET_PROPERTY_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_remove_property = {
    let key = alloc_key(scope, STYLE_REMOVE_PROPERTY_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_css_text_get = {
    let key = alloc_key(scope, STYLE_CSS_TEXT_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_css_text_set = {
    let key = alloc_key(scope, STYLE_CSS_TEXT_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_display_get = {
    let key = alloc_key(scope, STYLE_DISPLAY_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_display_set = {
    let key = alloc_key(scope, STYLE_DISPLAY_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_cursor_get = {
    let key = alloc_key(scope, STYLE_CURSOR_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_cursor_set = {
    let key = alloc_key(scope, STYLE_CURSOR_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_height_get = {
    let key = alloc_key(scope, STYLE_HEIGHT_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_height_set = {
    let key = alloc_key(scope, STYLE_HEIGHT_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_width_get = {
    let key = alloc_key(scope, STYLE_WIDTH_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let style_width_set = {
    let key = alloc_key(scope, STYLE_WIDTH_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  scope.define_property(
    wrapper,
    node_id_key,
    data_desc(Value::Number(node_id.index() as f64)),
  )?;

  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  scope.define_property(
    wrapper,
    wrapper_document_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(document_obj),
        writable: false,
      },
    },
  )?;

  if let Value::Number(_) = dom_source_id_value {
    scope.define_property(wrapper, dom_source_id_key, data_desc(dom_source_id_value))?;
  }

  if let Some(Value::Object(func)) = element_query_selector {
    let key = alloc_key(scope, "querySelector")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = element_query_selector_all {
    let key = alloc_key(scope, "querySelectorAll")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = element_matches {
    let key = alloc_key(scope, "matches")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = element_closest {
    let key = alloc_key(scope, "closest")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (class_name_get, class_name_set) {
    let class_name_key = alloc_key(scope, "className")?;
    scope.define_property(
      wrapper,
      class_name_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (id_get, id_set) {
    let id_key = alloc_key(scope, "id")?;
    scope.define_property(
      wrapper,
      id_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (src_get, src_set) {
    let key = alloc_key(scope, "src")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (srcset_get, srcset_set) {
    let key = alloc_key(scope, "srcset")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (sizes_get, sizes_set) {
    let key = alloc_key(scope, "sizes")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (href_get, href_set) {
    let key = alloc_key(scope, "href")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (rel_get, rel_set) {
    let key = alloc_key(scope, "rel")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (type_get, type_set) {
    let key = alloc_key(scope, "type")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (charset_get, charset_set) {
    let key = alloc_key(scope, "charset")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (cross_origin_get, cross_origin_set) {
    let key = alloc_key(scope, "crossOrigin")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (async_get, async_set) {
    let key = alloc_key(scope, "async")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (defer_get, defer_set) {
    let key = alloc_key(scope, "defer")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (height_get, height_set) {
    let key = alloc_key(scope, "height")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (width_get, width_set) {
    let key = alloc_key(scope, "width")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let Value::Number(_) = dom_source_id_value {
    if let (
      Some(Value::Object(add)),
      Some(Value::Object(remove)),
      Some(Value::Object(toggle)),
      Some(Value::Object(contains)),
      Some(Value::Object(replace)),
    ) = (
      class_list_add,
      class_list_remove,
      class_list_toggle,
      class_list_contains,
      class_list_replace,
    ) {
      let class_list = scope.alloc_object()?;
      scope.push_root(Value::Object(class_list))?;

      let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
      scope.define_property(
        class_list,
        node_id_key,
        data_desc(Value::Number(node_id.index() as f64)),
      )?;

      let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
      scope.define_property(class_list, source_id_key, data_desc(dom_source_id_value))?;

      let add_key = alloc_key(scope, "add")?;
      scope.define_property(class_list, add_key, data_desc(Value::Object(add)))?;
      let remove_key = alloc_key(scope, "remove")?;
      scope.define_property(class_list, remove_key, data_desc(Value::Object(remove)))?;
      let toggle_key = alloc_key(scope, "toggle")?;
      scope.define_property(class_list, toggle_key, data_desc(Value::Object(toggle)))?;
      let contains_key = alloc_key(scope, "contains")?;
      scope.define_property(class_list, contains_key, data_desc(Value::Object(contains)))?;
      let replace_key = alloc_key(scope, "replace")?;
      scope.define_property(class_list, replace_key, data_desc(Value::Object(replace)))?;

      let key = alloc_key(scope, "classList")?;
      scope.define_property(wrapper, key, data_desc(Value::Object(class_list)))?;
    }
  }

  // `Element.dataset` (DOMStringMap-like): implemented via host exotic property hooks so
  // `el.dataset.fooBar = "x"` reflects to `data-foo-bar="x"`.
  if let Value::Number(source_id_value) = dom_source_id_value {
    if let Some(dom_ptr) = dom_for_source(source_id_value as u64) {
      // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for
      // the lifetime of the associated host document.
      let dom = unsafe { dom_ptr.as_ref() };
      if matches!(
        dom.node(node_id).kind,
        dom2::NodeKind::Element { .. } | dom2::NodeKind::Slot { .. }
      ) {
        let dataset = scope.alloc_object()?;
        scope.push_root(Value::Object(dataset))?;
        scope.heap_mut().object_set_host_slots(
          dataset,
          HostSlots {
            a: node_id.index() as u64,
            b: DOM_STRING_MAP_HOST_KIND,
          },
        )?;

        let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
        scope.define_property(dataset, source_id_key, data_desc(dom_source_id_value))?;

        let key = alloc_key(scope, "dataset")?;
        scope.define_property(wrapper, key, data_desc(Value::Object(dataset)))?;
      }
    }
  }

  if let Value::Number(_) = dom_source_id_value {
    if let (
      Some(Value::Object(get_property_value)),
      Some(Value::Object(set_property)),
      Some(Value::Object(remove_property)),
      Some(Value::Object(css_text_get)),
      Some(Value::Object(css_text_set)),
      Some(Value::Object(display_get)),
      Some(Value::Object(display_set)),
      Some(Value::Object(cursor_get)),
      Some(Value::Object(cursor_set)),
      Some(Value::Object(height_get)),
      Some(Value::Object(height_set)),
      Some(Value::Object(width_get)),
      Some(Value::Object(width_set)),
    ) = (
      style_get_property_value,
      style_set_property,
      style_remove_property,
      style_css_text_get,
      style_css_text_set,
      style_display_get,
      style_display_set,
      style_cursor_get,
      style_cursor_set,
      style_height_get,
      style_height_set,
      style_width_get,
      style_width_set,
    ) {
      let style = scope.alloc_object()?;
      scope.push_root(Value::Object(style))?;

      let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
      scope.define_property(
        style,
        node_id_key,
        data_desc(Value::Number(node_id.index() as f64)),
      )?;

      let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
      scope.define_property(style, source_id_key, data_desc(dom_source_id_value))?;

      let get_property_value_key = alloc_key(scope, "getPropertyValue")?;
      scope.define_property(
        style,
        get_property_value_key,
        data_desc(Value::Object(get_property_value)),
      )?;

      let set_property_key = alloc_key(scope, "setProperty")?;
      scope.define_property(style, set_property_key, data_desc(Value::Object(set_property)))?;

      let remove_property_key = alloc_key(scope, "removeProperty")?;
      scope.define_property(
        style,
        remove_property_key,
        data_desc(Value::Object(remove_property)),
      )?;

      let css_text_key = alloc_key(scope, "cssText")?;
      scope.define_property(
        style,
        css_text_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(css_text_get),
            set: Value::Object(css_text_set),
          },
        },
      )?;

      let display_key = alloc_key(scope, "display")?;
      scope.define_property(
        style,
        display_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(display_get),
            set: Value::Object(display_set),
          },
        },
      )?;

      let cursor_key = alloc_key(scope, "cursor")?;
      scope.define_property(
        style,
        cursor_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(cursor_get),
            set: Value::Object(cursor_set),
          },
        },
      )?;

      let height_key = alloc_key(scope, "height")?;
      scope.define_property(
        style,
        height_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(height_get),
            set: Value::Object(height_set),
          },
        },
      )?;

      let width_key = alloc_key(scope, "width")?;
      scope.define_property(
        style,
        width_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(width_get),
            set: Value::Object(width_set),
          },
        },
      )?;

      let key = alloc_key(scope, "style")?;
      scope.define_property(wrapper, key, data_desc(Value::Object(style)))?;
    }
  }

  if let Some(Value::Object(func)) = append_child {
    let key = alloc_key(scope, "appendChild")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = insert_before {
    let key = alloc_key(scope, "insertBefore")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = remove_child {
    let key = alloc_key(scope, "removeChild")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = replace_child {
    let key = alloc_key(scope, "replaceChild")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = clone_node {
    let key = alloc_key(scope, "cloneNode")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(get)) = parent_node_get {
    let key = alloc_key(scope, "parentNode")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Undefined,
        },
      },
    )?;
  }

  if let Some(Value::Object(get)) = first_child_get {
    let key = alloc_key(scope, "firstChild")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Undefined,
        },
      },
    )?;
  }

  if let Some(Value::Object(get)) = previous_sibling_get {
    let key = alloc_key(scope, "previousSibling")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Undefined,
        },
      },
    )?;
  }

  if let Some(Value::Object(get)) = next_sibling_get {
    let key = alloc_key(scope, "nextSibling")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Undefined,
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (text_content_get, text_content_set) {
    let key = alloc_key(scope, "textContent")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let Some(Value::Object(func)) = node_remove {
    let key = alloc_key(scope, "remove")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = add_event_listener {
    let key = alloc_key(scope, "addEventListener")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = remove_event_listener {
    let key = alloc_key(scope, "removeEventListener")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = dispatch_event {
    let key = alloc_key(scope, "dispatchEvent")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = get_attribute {
    let key = alloc_key(scope, "getAttribute")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = set_attribute {
    let key = alloc_key(scope, "setAttribute")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = remove_attribute {
    let key = alloc_key(scope, "removeAttribute")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (inner_html_get, inner_html_set) {
    let key = alloc_key(scope, "innerHTML")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (outer_html_get, outer_html_set) {
    let key = alloc_key(scope, "outerHTML")?;
    scope.define_property(
      wrapper,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(get),
          set: Value::Object(set),
        },
      },
    )?;
  }

  if let Some(Value::Object(func)) = insert_adjacent_html {
    let key = alloc_key(scope, "insertAdjacentHTML")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = insert_adjacent_element {
    let key = alloc_key(scope, "insertAdjacentElement")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = insert_adjacent_text {
    let key = alloc_key(scope, "insertAdjacentText")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  Ok(Value::Object(wrapper))
}

fn node_wrapper_document_obj(
  scope: &mut Scope<'_>,
  wrapper_obj: GcObject,
  node_id: NodeId,
) -> Result<GcObject, VmError> {
  if node_id.index() == 0 {
    // `window.document` is the canonical wrapper for the dom2 document node.
    return Ok(wrapper_obj);
  }

  let key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &key)?
  {
    Some(Value::Object(obj)) => Ok(obj),
    _ => Err(VmError::TypeError("Illegal invocation")),
  }
}

fn sync_child_nodes_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  dom_source_id: u64,
  document_obj: GcObject,
  node_id: NodeId,
  array: GcObject,
) -> Result<(), VmError> {
  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError(
      "Node.childNodes requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let mut children: Vec<NodeId> = Vec::new();
  children
    .try_reserve(dom.node(node_id).children.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for &child in dom.node(node_id).children.iter() {
    if child.index() >= dom.nodes_len() {
      continue;
    }
    if dom.node(child).parent != Some(node_id) {
      continue;
    }
    children.push(child);
  }

  // Root objects while allocating property keys.
  scope.push_root(Value::Object(document_obj))?;
  scope.push_root(Value::Object(array))?;

  let length_key = alloc_key(scope, "length")?;
  let old_len = match scope.heap().object_get_own_data_property_value(array, &length_key)? {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => 0,
  };

  // Overwrite / populate indices.
  let mut idx_buf = [0u8; 20];
  for (idx, child_id) in children.iter().copied().enumerate() {
    let idx_str = decimal_str_for_usize(idx, &mut idx_buf);
    let key = alloc_key(scope, idx_str)?;
    let wrapper = get_or_create_node_wrapper(vm, scope, document_obj, child_id)?;
    scope.define_property(array, key, data_desc(wrapper))?;
  }

  // Delete leftover indices when the list shrinks.
  for idx in children.len()..old_len {
    let idx_str = decimal_str_for_usize(idx, &mut idx_buf);
    let key = alloc_key(scope, idx_str)?;
    scope.heap_mut().delete_property_or_throw(array, key)?;
  }

  scope.define_property(
    array,
    length_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(children.len() as f64),
        writable: true,
      },
    },
  )?;

  Ok(())
}

fn sync_cached_child_nodes_for_wrapper(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  dom_source_id: u64,
  document_obj: GcObject,
  wrapper_obj: GcObject,
  node_id: NodeId,
) -> Result<(), VmError> {
  let key = alloc_key(scope, NODE_CHILD_NODES_KEY)?;
  let Some(Value::Object(array)) = scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &key)?
  else {
    return Ok(());
  };
  sync_child_nodes_array(vm, scope, dom_source_id, document_obj, node_id, array)
}

fn sync_cached_child_nodes_for_node_id(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  dom_source_id: u64,
  document_obj: GcObject,
  node_id: NodeId,
) -> Result<(), VmError> {
  let wrapper_obj = if node_id.index() == 0 {
    Some(document_obj)
  } else {
    dom_platform_mut(vm).and_then(|platform| platform.get_existing_wrapper(scope.heap(), node_id))
  };
  let Some(wrapper_obj) = wrapper_obj else {
    return Ok(());
  };
  sync_cached_child_nodes_for_wrapper(vm, scope, dom_source_id, document_obj, wrapper_obj, node_id)
}

fn document_document_element_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Null),
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Null);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let Some(node_id) = dom.document_element() else {
    return Ok(Value::Null);
  };

  get_or_create_node_wrapper(vm, scope, document_obj, node_id)
}

fn document_head_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Null),
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Null);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let Some(node_id) = dom.head() else {
    return Ok(Value::Null);
  };

  get_or_create_node_wrapper(vm, scope, document_obj, node_id)
}

fn document_body_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Null),
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Null);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let Some(node_id) = dom.body() else {
    return Ok(Value::Null);
  };

  get_or_create_node_wrapper(vm, scope, document_obj, node_id)
}

fn document_get_element_by_id_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Null),
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Null);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let query_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let query_value = scope.heap_mut().to_string(query_value)?;
  let query = scope
    .heap()
    .get_string(query_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(node_id) = dom.get_element_by_id(&query) else {
    return Ok(Value::Null);
  };
  get_or_create_node_wrapper(vm, scope, document_obj, node_id)
}

fn document_fragment_get_element_by_id_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, fragment_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let fragment_id =
      platform.require_document_fragment_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), fragment_id)
  };

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let query_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let query_value = scope.heap_mut().to_string(query_value)?;
  let query = scope
    .heap()
    .get_string(query_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(found) = dom.get_element_by_id_from(fragment_id, &query) else {
    return Ok(Value::Null);
  };

  let document_obj = node_wrapper_document_obj(scope, wrapper_obj, fragment_id)?;
  get_or_create_node_wrapper(vm, scope, document_obj, found)
}

fn document_query_selector_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Null),
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Null);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };

  let selector_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let selector_value = scope.heap_mut().to_string(selector_value)?;
  let selector = scope
    .heap()
    .get_string(selector_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  match dom.query_selector(&selector, None) {
    Ok(Some(node_id)) => get_or_create_node_wrapper(vm, scope, document_obj, node_id),
    Ok(None) => Ok(Value::Null),
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => {
          ("NotSupportedError", message)
        }
        crate::web::dom::DomException::InvalidStateError { message } => {
          ("InvalidStateError", message)
        }
      };
      Err(VmError::Throw(make_dom_exception(scope, name, &message)?))
    }
  }
}

fn document_query_selector_all_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Null),
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Null);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };

  let selector_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let selector_value = scope.heap_mut().to_string(selector_value)?;
  let selector = scope
    .heap()
    .get_string(selector_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let matches = match dom.query_selector_all(&selector, None) {
    Ok(nodes) => nodes,
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => {
          ("NotSupportedError", message)
        }
        crate::web::dom::DomException::InvalidStateError { message } => {
          ("InvalidStateError", message)
        }
      };
      return Err(VmError::Throw(make_dom_exception(scope, name, &message)?));
    }
  };

  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  if let Some(intrinsics) = vm.intrinsics() {
    scope
      .heap_mut()
      .object_set_prototype(array, Some(intrinsics.array_prototype()))?;
  }

  for (idx, node_id) in matches.iter().copied().enumerate() {
    let key = alloc_key(scope, &idx.to_string())?;
    let wrapper = get_or_create_node_wrapper(vm, scope, document_obj, node_id)?;
    scope.define_property(array, key, data_desc(wrapper))?;
  }

  let length_key = alloc_key(scope, "length")?;
  scope.define_property(
    array,
    length_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(matches.len() as f64),
        writable: true,
      },
    },
  )?;

  Ok(Value::Object(array))
}

fn element_query_selector_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.querySelector must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.querySelector requires a DOM-backed element",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.querySelector must be called on a node object",
      ));
    }
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.querySelector requires a DOM-backed element",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Null);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => {
      return Err(VmError::TypeError(
        "Element.querySelector must be called on a node object",
      ));
    }
  };

  let selector_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let selector_value = scope.heap_mut().to_string(selector_value)?;
  let selector = scope
    .heap()
    .get_string(selector_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  match dom.query_selector(&selector, Some(node_id)) {
    Ok(Some(found)) => get_or_create_node_wrapper(vm, scope, document_obj, found),
    Ok(None) => Ok(Value::Null),
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => {
          ("NotSupportedError", message)
        }
        crate::web::dom::DomException::InvalidStateError { message } => {
          ("InvalidStateError", message)
        }
      };
      Err(VmError::Throw(make_dom_exception(scope, name, &message)?))
    }
  }
}

fn element_query_selector_all_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.querySelectorAll must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.querySelectorAll requires a DOM-backed element",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.querySelectorAll must be called on a node object",
      ));
    }
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.querySelectorAll requires a DOM-backed element",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.querySelectorAll requires a DOM-backed element",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.querySelectorAll must be called on a node object"))?;

  let selector_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let selector_value = scope.heap_mut().to_string(selector_value)?;
  let selector = scope
    .heap()
    .get_string(selector_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let matches = match dom.query_selector_all(&selector, Some(node_id)) {
    Ok(nodes) => nodes,
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => {
          ("NotSupportedError", message)
        }
        crate::web::dom::DomException::InvalidStateError { message } => {
          ("InvalidStateError", message)
        }
      };
      return Err(VmError::Throw(make_dom_exception(scope, name, &message)?));
    }
  };

  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  if let Some(intrinsics) = vm.intrinsics() {
    scope
      .heap_mut()
      .object_set_prototype(array, Some(intrinsics.array_prototype()))?;
  }

  for (idx, node_id) in matches.iter().copied().enumerate() {
    let key = alloc_key(scope, &idx.to_string())?;
    let wrapper = get_or_create_node_wrapper(vm, scope, document_obj, node_id)?;
    scope.define_property(array, key, data_desc(wrapper))?;
  }

  let length_key = alloc_key(scope, "length")?;
  scope.define_property(
    array,
    length_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(matches.len() as f64),
        writable: true,
      },
    },
  )?;

  Ok(Value::Object(array))
}

fn element_matches_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.matches must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.matches requires a DOM-backed element",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.matches must be called on a node object",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.matches requires a DOM-backed element",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.matches must be called on a node object"))?;

  let selector_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let selector_value = scope.heap_mut().to_string(selector_value)?;
  let selector = scope
    .heap()
    .get_string(selector_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  match dom.matches_selector(node_id, &selector) {
    Ok(result) => Ok(Value::Bool(result)),
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => {
          ("NotSupportedError", message)
        }
        crate::web::dom::DomException::InvalidStateError { message } => {
          ("InvalidStateError", message)
        }
      };
      Err(VmError::Throw(make_dom_exception(scope, name, &message)?))
    }
  }
}

fn element_closest_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.closest must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.closest requires a DOM-backed element",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.closest must be called on a node object",
      ));
    }
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.closest requires a DOM-backed element",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.closest requires a DOM-backed element",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.closest must be called on a node object"))?;

  let selector_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let selector_value = scope.heap_mut().to_string(selector_value)?;
  let selector = scope
    .heap()
    .get_string(selector_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  match dom.closest(node_id, &selector) {
    Ok(Some(found)) => get_or_create_node_wrapper(vm, scope, document_obj, found),
    Ok(None) => Ok(Value::Null),
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => {
          ("NotSupportedError", message)
        }
        crate::web::dom::DomException::InvalidStateError { message } => {
          ("InvalidStateError", message)
        }
      };
      Err(VmError::Throw(make_dom_exception(scope, name, &message)?))
    }
  }
}

fn document_create_element_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Err(VmError::TypeError(
      "document.createElement must be called on a document object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "document.createElement requires a DOM-backed document",
      ));
    }
  };

  let tag_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let tag_value = scope.heap_mut().to_string(tag_value)?;
  let tag_name = scope
    .heap()
    .get_string(tag_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "document.createElement requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom.create_element(&tag_name, "");

  get_or_create_node_wrapper(vm, scope, document_obj, node_id)
}

fn document_create_text_node_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Err(VmError::TypeError(
      "document.createTextNode must be called on a document object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "document.createTextNode requires a DOM-backed document",
      ));
    }
  };

  let data_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let data_value = scope.heap_mut().to_string(data_value)?;
  let data = scope
    .heap()
    .get_string(data_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "document.createTextNode requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom.create_text(&data);

  get_or_create_node_wrapper(vm, scope, document_obj, node_id)
}

fn document_create_comment_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Err(VmError::TypeError(
      "document.createComment must be called on a document object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "document.createComment requires a DOM-backed document",
      ));
    }
  };

  let data_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let data_value = scope.heap_mut().to_string(data_value)?;
  let data = scope
    .heap()
    .get_string(data_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "document.createComment requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom.create_comment(&data);

  get_or_create_node_wrapper(vm, scope, document_obj, node_id)
}

fn document_create_document_fragment_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Err(VmError::TypeError(
      "document.createDocumentFragment must be called on a document object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "document.createDocumentFragment requires a DOM-backed document",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "document.createDocumentFragment requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom.create_document_fragment();

  get_or_create_node_wrapper(vm, scope, document_obj, node_id)
}

fn event_constructor_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Note: for MVP, we do not enforce `new` (calling as a function also produces a new object), which
  // matches `src/js/dom_bindings` behavior.
  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_string = scope.heap_mut().to_string(type_arg)?;

  let mut bubbles = false;
  let mut cancelable = false;
  let mut composed = false;
  if let Some(init_value) = args.get(1).copied() {
    if let Value::Object(init_obj) = init_value {
      let bubbles_key = alloc_key(scope, "bubbles")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &bubbles_key)?
      {
        bubbles = scope.heap().to_boolean(value)?;
      }

      let cancelable_key = alloc_key(scope, "cancelable")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &cancelable_key)?
      {
        cancelable = scope.heap().to_boolean(value)?;
      }

      let composed_key = alloc_key(scope, "composed")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &composed_key)?
      {
        composed = scope.heap().to_boolean(value)?;
      }
    }
  }

  let prototype_key = alloc_key(scope, "prototype")?;
  let proto = scope
    .heap()
    .object_get_own_data_property_value(callee, &prototype_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  if let Some(proto) = proto {
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  }

  let type_key = alloc_key(scope, "type")?;
  scope.define_property(obj, type_key, data_desc(Value::String(type_string)))?;

  let bubbles_key = alloc_key(scope, "bubbles")?;
  scope.define_property(obj, bubbles_key, data_desc(Value::Bool(bubbles)))?;

  let cancelable_key = alloc_key(scope, "cancelable")?;
  scope.define_property(obj, cancelable_key, data_desc(Value::Bool(cancelable)))?;

  let composed_key = alloc_key(scope, "composed")?;
  scope.define_property(obj, composed_key, data_desc(Value::Bool(composed)))?;

  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  scope.define_property(obj, default_prevented_key, data_desc(Value::Bool(false)))?;

  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(obj, cancel_bubble_key, data_desc(Value::Bool(false)))?;

  Ok(Value::Object(obj))
}

fn custom_event_constructor_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_string = scope.heap_mut().to_string(type_arg)?;

  let mut bubbles = false;
  let mut cancelable = false;
  let mut composed = false;
  let mut detail = Value::Null;
  if let Some(init_value) = args.get(1).copied() {
    if let Value::Object(init_obj) = init_value {
      let bubbles_key = alloc_key(scope, "bubbles")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &bubbles_key)?
      {
        bubbles = scope.heap().to_boolean(value)?;
      }

      let cancelable_key = alloc_key(scope, "cancelable")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &cancelable_key)?
      {
        cancelable = scope.heap().to_boolean(value)?;
      }

      let composed_key = alloc_key(scope, "composed")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &composed_key)?
      {
        composed = scope.heap().to_boolean(value)?;
      }

      let detail_key = alloc_key(scope, "detail")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &detail_key)?
      {
        if !matches!(value, Value::Undefined) {
          detail = value;
        }
      }
    }
  }

  let prototype_key = alloc_key(scope, "prototype")?;
  let proto = scope
    .heap()
    .object_get_own_data_property_value(callee, &prototype_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  if let Some(proto) = proto {
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  }

  let type_key = alloc_key(scope, "type")?;
  scope.define_property(obj, type_key, data_desc(Value::String(type_string)))?;

  let bubbles_key = alloc_key(scope, "bubbles")?;
  scope.define_property(obj, bubbles_key, data_desc(Value::Bool(bubbles)))?;

  let cancelable_key = alloc_key(scope, "cancelable")?;
  scope.define_property(obj, cancelable_key, data_desc(Value::Bool(cancelable)))?;

  let composed_key = alloc_key(scope, "composed")?;
  scope.define_property(obj, composed_key, data_desc(Value::Bool(composed)))?;

  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  scope.define_property(obj, default_prevented_key, data_desc(Value::Bool(false)))?;

  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(obj, cancel_bubble_key, data_desc(Value::Bool(false)))?;

  let detail_key = alloc_key(scope, "detail")?;
  scope.define_property(obj, detail_key, data_desc(detail))?;

  Ok(Value::Object(obj))
}

fn event_constructor_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };
  event_constructor_native(vm, scope, host, hooks, ctor, Value::Undefined, args)
}

fn custom_event_constructor_construct_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };
  custom_event_constructor_native(vm, scope, host, hooks, ctor, Value::Undefined, args)
}

fn event_init_event_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(event_obj) = this else {
    return Err(VmError::TypeError(
      "Event.initEvent must be called on an Event object",
    ));
  };

  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_string = scope.heap_mut().to_string(type_arg)?;

  let bubbles_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  let bubbles = scope.heap().to_boolean(bubbles_arg)?;

  let cancelable_arg = args.get(2).copied().unwrap_or(Value::Undefined);
  let cancelable = scope.heap().to_boolean(cancelable_arg)?;

  let type_key = alloc_key(scope, "type")?;
  scope.define_property(event_obj, type_key, data_desc(Value::String(type_string)))?;

  let bubbles_key = alloc_key(scope, "bubbles")?;
  scope.define_property(event_obj, bubbles_key, data_desc(Value::Bool(bubbles)))?;

  let cancelable_key = alloc_key(scope, "cancelable")?;
  scope.define_property(
    event_obj,
    cancelable_key,
    data_desc(Value::Bool(cancelable)),
  )?;

  // `initEvent` does not expose `composed`; reset to false per DOM.
  let composed_key = alloc_key(scope, "composed")?;
  scope.define_property(event_obj, composed_key, data_desc(Value::Bool(false)))?;

  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  scope.define_property(
    event_obj,
    default_prevented_key,
    data_desc(Value::Bool(false)),
  )?;

  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(event_obj, cancel_bubble_key, data_desc(Value::Bool(false)))?;

  Ok(Value::Undefined)
}

fn custom_event_init_custom_event_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(event_obj) = this else {
    return Err(VmError::TypeError(
      "CustomEvent.initCustomEvent must be called on a CustomEvent object",
    ));
  };

  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_string = scope.heap_mut().to_string(type_arg)?;

  let bubbles_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  let bubbles = scope.heap().to_boolean(bubbles_arg)?;

  let cancelable_arg = args.get(2).copied().unwrap_or(Value::Undefined);
  let cancelable = scope.heap().to_boolean(cancelable_arg)?;

  let detail_arg = args.get(3).copied().unwrap_or(Value::Undefined);
  let detail = if matches!(detail_arg, Value::Undefined) {
    Value::Null
  } else {
    detail_arg
  };

  let type_key = alloc_key(scope, "type")?;
  scope.define_property(event_obj, type_key, data_desc(Value::String(type_string)))?;

  let bubbles_key = alloc_key(scope, "bubbles")?;
  scope.define_property(event_obj, bubbles_key, data_desc(Value::Bool(bubbles)))?;

  let cancelable_key = alloc_key(scope, "cancelable")?;
  scope.define_property(
    event_obj,
    cancelable_key,
    data_desc(Value::Bool(cancelable)),
  )?;

  // `initCustomEvent` does not expose `composed`; reset to false per DOM.
  let composed_key = alloc_key(scope, "composed")?;
  scope.define_property(event_obj, composed_key, data_desc(Value::Bool(false)))?;

  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  scope.define_property(
    event_obj,
    default_prevented_key,
    data_desc(Value::Bool(false)),
  )?;

  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(event_obj, cancel_bubble_key, data_desc(Value::Bool(false)))?;

  let detail_key = alloc_key(scope, "detail")?;
  scope.define_property(event_obj, detail_key, data_desc(detail))?;

  Ok(Value::Undefined)
}

fn event_prototype_prevent_default_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(event_obj) = this else {
    return Err(VmError::TypeError(
      "Event.preventDefault must be called on an Event object",
    ));
  };

  if let Some(event_id) = event_active_event_id(scope, event_obj)? {
    if with_active_dom_event(event_id, |event| event.prevent_default()).is_some() {
      let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
      let default_prevented =
        with_active_dom_event(event_id, |event| event.default_prevented).unwrap_or(false);
      scope.define_property(
        event_obj,
        default_prevented_key,
        data_desc(Value::Bool(default_prevented)),
      )?;
      return Ok(Value::Undefined);
    }
  }

  let cancelable_key = alloc_key(scope, "cancelable")?;
  let cancelable = scope
    .heap()
    .object_get_own_data_property_value(event_obj, &cancelable_key)?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?
    .unwrap_or(false);
  if !cancelable {
    return Ok(Value::Undefined);
  }
  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  scope.define_property(
    event_obj,
    default_prevented_key,
    data_desc(Value::Bool(true)),
  )?;
  Ok(Value::Undefined)
}

fn event_prototype_stop_propagation_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(event_obj) = this else {
    return Err(VmError::TypeError(
      "Event.stopPropagation must be called on an Event object",
    ));
  };

  if let Some(event_id) = event_active_event_id(scope, event_obj)? {
    if with_active_dom_event(event_id, |event| event.stop_propagation()).is_some() {
      let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
      scope.define_property(event_obj, cancel_bubble_key, data_desc(Value::Bool(true)))?;
      return Ok(Value::Undefined);
    }
  }

  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(event_obj, cancel_bubble_key, data_desc(Value::Bool(true)))?;
  Ok(Value::Undefined)
}

fn event_prototype_stop_immediate_propagation_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(event_obj) = this else {
    return Err(VmError::TypeError(
      "Event.stopImmediatePropagation must be called on an Event object",
    ));
  };

  if let Some(event_id) = event_active_event_id(scope, event_obj)? {
    if with_active_dom_event(event_id, |event| event.stop_immediate_propagation()).is_some() {
      let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
      scope.define_property(event_obj, cancel_bubble_key, data_desc(Value::Bool(true)))?;
      return Ok(Value::Undefined);
    }
  }

  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(event_obj, cancel_bubble_key, data_desc(Value::Bool(true)))?;
  Ok(Value::Undefined)
}

fn event_target_constructor_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "EventTarget constructor cannot be invoked without 'new'",
  ))
}

fn event_target_constructor_construct_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
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
  // Brand-check for EventTarget.prototype methods (so borrowing the methods onto random objects
  // throws, matching web platform behavior).
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
  Ok(Value::Object(obj))
}

fn event_target_default_this_from_callee(
  scope: &Scope<'_>,
  callee: GcObject,
) -> Result<Option<GcObject>, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  Ok(match slots
    .get(EVENT_TARGET_DEFAULT_THIS_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => Some(obj),
    _ => None,
  })
}

fn event_target_context_global_from_callee(
  scope: &Scope<'_>,
  callee: GcObject,
) -> Result<GcObject, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(EVENT_TARGET_CONTEXT_GLOBAL_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "EventTarget method missing required global slot",
    )),
  }
}

fn event_target_resolve_this(
  scope: &Scope<'_>,
  callee: GcObject,
  this: Value,
) -> Result<GcObject, VmError> {
  match this {
    Value::Object(obj) => Ok(obj),
    Value::Undefined | Value::Null => match event_target_default_this_from_callee(scope, callee)? {
      Some(obj) => Ok(obj),
      None => Err(VmError::TypeError("Illegal invocation")),
    },
    _ => Err(VmError::TypeError("Illegal invocation")),
  }
}

struct ResolvedDomEventTarget {
  window_obj: GcObject,
  document_obj: GcObject,
  dom_source_id: u64,
  target_id: web_events::EventTargetId,
}

fn dom_source_id_from_document(
  scope: &mut Scope<'_>,
  document_obj: GcObject,
) -> Result<Option<u64>, VmError> {
  let key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &key)?
  {
    None => Ok(None),
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => Ok(Some(n as u64)),
    _ => Err(VmError::TypeError("EventTarget method requires a DOM-backed document")),
  }
}

fn window_object_from_document(scope: &mut Scope<'_>, document_obj: GcObject) -> Result<GcObject, VmError> {
  let key = alloc_key(scope, DOCUMENT_WINDOW_KEY)?;
  match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &key)?
  {
    Some(Value::Object(obj)) => Ok(obj),
    _ => Err(VmError::InvariantViolation(
      "document is missing required window backreference",
    )),
  }
}

fn resolve_dom_event_target(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  target_obj: GcObject,
) -> Result<(ResolvedDomEventTarget, NonNull<dom2::Document>), VmError> {
  // Node wrapper: has a backreference to its owning document object.
  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  if let Some(Value::Object(document_obj)) = scope
    .heap()
    .object_get_own_data_property_value(target_obj, &wrapper_document_key)?
  {
    let window_obj = window_object_from_document(scope, document_obj)?;
    let dom_source_id = dom_source_id_from_document(scope, document_obj)?.ok_or(VmError::TypeError(
      "EventTarget method requires a DOM-backed document",
    ))?;
    let Some(mut dom_ptr) = dom_for_source(dom_source_id) else {
      return Err(VmError::TypeError(
        "EventTarget method requires a DOM-backed document",
      ));
    };
    // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for
    // the lifetime of the associated host document.
    let dom = unsafe { dom_ptr.as_mut() };

    let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
    let node_index = match scope
      .heap()
      .object_get_own_data_property_value(target_obj, &node_id_key)?
    {
      Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
      _ => {
        return Err(VmError::TypeError(
          "EventTarget method requires a node-backed event target",
        ))
      }
    };
    let node_id = dom
      .node_id_from_index(node_index)
      .map_err(|_| VmError::TypeError("EventTarget method requires a node-backed event target"))?;

    return Ok((
      ResolvedDomEventTarget {
        window_obj,
        document_obj,
        dom_source_id,
        target_id: web_events::EventTargetId::Node(node_id).normalize(),
      },
      dom_ptr,
    ));
  }

  // Document event target: identified by the presence of the internal window backreference.
  let window_key = alloc_key(scope, DOCUMENT_WINDOW_KEY)?;
  if matches!(
    scope
      .heap()
      .object_get_own_data_property_value(target_obj, &window_key)?,
    Some(Value::Object(_))
  ) {
    let document_obj = target_obj;
    let window_obj = window_object_from_document(scope, document_obj)?;
    let dom_source_id = dom_source_id_from_document(scope, document_obj)?;
    let dom_ptr = if let Some(dom_source_id) = dom_source_id {
      dom_for_source(dom_source_id).ok_or(VmError::TypeError(
        "EventTarget method requires a DOM-backed document",
      ))?
    } else {
      let Some(user_data) = vm.user_data_mut::<WindowRealmUserData>() else {
        return Err(VmError::InvariantViolation(
          "WindowRealm is missing required VM user data",
        ));
      };
      NonNull::from(&mut user_data.events_dom_fallback)
    };
    return Ok((
      ResolvedDomEventTarget {
        window_obj,
        document_obj,
        dom_source_id: dom_source_id.unwrap_or(0),
        target_id: web_events::EventTargetId::Document,
      },
      dom_ptr,
    ));
  }

  // Window event target: has an own `document` property that points at the document shim.
  let document_key = alloc_key(scope, "document")?;
  if let Some(Value::Object(document_obj)) = scope
    .heap()
    .object_get_own_data_property_value(target_obj, &document_key)?
  {
    let window_key = alloc_key(scope, DOCUMENT_WINDOW_KEY)?;
    if matches!(
      scope
      .heap()
      .object_get_own_data_property_value(document_obj, &window_key)?,
      Some(Value::Object(_))
    ) {
      let dom_source_id = dom_source_id_from_document(scope, document_obj)?;
      let dom_ptr = if let Some(dom_source_id) = dom_source_id {
        dom_for_source(dom_source_id).ok_or(VmError::TypeError(
          "EventTarget method requires a DOM-backed document",
        ))?
      } else {
        let Some(user_data) = vm.user_data_mut::<WindowRealmUserData>() else {
          return Err(VmError::InvariantViolation(
            "WindowRealm is missing required VM user data",
          ));
        };
        NonNull::from(&mut user_data.events_dom_fallback)
      };
      return Ok((
        ResolvedDomEventTarget {
          window_obj: target_obj,
          document_obj,
          dom_source_id: dom_source_id.unwrap_or(0),
          target_id: web_events::EventTargetId::Window,
        },
        dom_ptr,
      ));
    }
  }

  Err(VmError::TypeError("Illegal invocation"))
}

struct ResolvedEventTarget {
  resolved: ResolvedDomEventTarget,
  dom_ptr: NonNull<dom2::Document>,
  listener_roots_owner: GcObject,
  opaque_target_obj: Option<GcObject>,
}

fn gc_object_id(obj: GcObject) -> u64 {
  (obj.index() as u64) | ((obj.generation() as u64) << 32)
}

fn is_branded_event_target(scope: &mut Scope<'_>, obj: GcObject) -> Result<bool, VmError> {
  let key = alloc_key(scope, EVENT_TARGET_BRAND_KEY)?;
  Ok(matches!(
    scope.heap().object_get_own_data_property_value(obj, &key)?,
    Some(Value::Bool(true))
  ))
}

fn resolve_event_target(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  callee: GcObject,
  target_obj: GcObject,
) -> Result<ResolvedEventTarget, VmError> {
  let (resolved_dom, dom_ptr) = match resolve_dom_event_target(vm, scope, target_obj) {
    Ok(ok) => ok,
    Err(err) => {
      // Non-DOM EventTarget objects (e.g. `AbortSignal`, `new EventTarget()`).
      if !is_branded_event_target(scope, target_obj)? {
        return Err(err);
      }

      let window_obj = event_target_context_global_from_callee(scope, callee)?;
      let document_key = alloc_key(scope, "document")?;
      let document_obj = match vm.get(scope, window_obj, document_key)? {
        Value::Object(obj) => obj,
        _ => return Err(VmError::TypeError("Illegal invocation")),
      };
      let dom_source_id = dom_source_id_from_document(scope, document_obj)?;
      let dom_ptr = if let Some(dom_source_id) = dom_source_id {
        dom_for_source(dom_source_id).ok_or(VmError::TypeError(
          "EventTarget method requires a DOM-backed document",
        ))?
      } else {
        let Some(user_data) = vm.user_data_mut::<WindowRealmUserData>() else {
          return Err(VmError::InvariantViolation(
            "WindowRealm is missing required VM user data",
          ));
        };
        NonNull::from(&mut user_data.events_dom_fallback)
      };

      return Ok(ResolvedEventTarget {
        listener_roots_owner: target_obj,
        resolved: ResolvedDomEventTarget {
          window_obj,
          document_obj,
          dom_source_id: dom_source_id.unwrap_or(0),
          target_id: web_events::EventTargetId::Opaque(gc_object_id(target_obj)),
        },
        dom_ptr,
        opaque_target_obj: Some(target_obj),
      });
    }
  };

  Ok(ResolvedEventTarget {
    listener_roots_owner: resolved_dom.document_obj,
    resolved: resolved_dom,
    dom_ptr,
    opaque_target_obj: None,
  })
}

fn parse_add_event_listener_options(
  scope: &mut Scope<'_>,
  value: Value,
) -> Result<web_events::AddEventListenerOptions, VmError> {
  let mut opts = web_events::AddEventListenerOptions::default();
  match value {
    Value::Bool(b) => {
      opts.capture = b;
      Ok(opts)
    }
    Value::Object(obj) => {
      let capture_key = alloc_key(scope, "capture")?;
      if let Some(v) = scope
        .heap()
        .object_get_own_data_property_value(obj, &capture_key)?
      {
        opts.capture = scope.heap().to_boolean(v)?;
      }

      let once_key = alloc_key(scope, "once")?;
      if let Some(v) = scope
        .heap()
        .object_get_own_data_property_value(obj, &once_key)?
      {
        opts.once = scope.heap().to_boolean(v)?;
      }

      let passive_key = alloc_key(scope, "passive")?;
      if let Some(v) = scope
        .heap()
        .object_get_own_data_property_value(obj, &passive_key)?
      {
        opts.passive = scope.heap().to_boolean(v)?;
      }

      Ok(opts)
    }
    _ => Ok(opts),
  }
}

fn parse_event_listener_capture(scope: &mut Scope<'_>, value: Value) -> Result<bool, VmError> {
  Ok(match value {
    Value::Bool(b) => b,
    Value::Object(obj) => {
      let capture_key = alloc_key(scope, "capture")?;
      match scope
        .heap()
        .object_get_own_data_property_value(obj, &capture_key)?
      {
        Some(v) => scope.heap().to_boolean(v)?,
        None => false,
      }
    }
    _ => false,
  })
}

fn get_or_create_event_listener_roots(scope: &mut Scope<'_>, owner_obj: GcObject) -> Result<GcObject, VmError> {
  let key = alloc_key(scope, EVENT_LISTENER_ROOTS_KEY)?;
  if let Some(Value::Object(obj)) = scope.heap().object_get_own_data_property_value(owner_obj, &key)?
  {
    return Ok(obj);
  }

  let roots = scope.alloc_object()?;
  scope.push_root(Value::Object(roots))?;
  scope.define_property(
    owner_obj,
    key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(roots),
        writable: false,
      },
    },
  )?;
  Ok(roots)
}

fn listener_id_property_key(
  scope: &mut Scope<'_>,
  listener_id: web_events::ListenerId,
) -> Result<PropertyKey, VmError> {
  let key = listener_id.get().to_string();
  let key_s = scope.alloc_string(&key)?;
  scope.push_root(Value::String(key_s))?;
  Ok(PropertyKey::from_string(key_s))
}

fn remove_listener_root_if_unused(
  scope: &mut Scope<'_>,
  roots_owner_obj: GcObject,
  registry: &web_events::EventListenerRegistry,
  listener_id: web_events::ListenerId,
  target_for_owner: Option<web_events::EventTargetId>,
) -> Result<(), VmError> {
  match target_for_owner {
    Some(target) => {
      if registry.contains_listener_id_for_target(target, listener_id) {
        return Ok(());
      }
    }
    None => {
      if registry.contains_listener_id(listener_id) {
        return Ok(());
      }
    }
  };

  let roots_key = alloc_key(scope, EVENT_LISTENER_ROOTS_KEY)?;
  let Some(Value::Object(roots)) = scope.heap().object_get_own_data_property_value(roots_owner_obj, &roots_key)?
  else {
    return Ok(());
  };
  let listener_key = listener_id_property_key(scope, listener_id)?;
  let _ = scope.ordinary_delete(roots, listener_key)?;
  Ok(())
}

struct ActiveDomEventGuard {
  event_id: u64,
}

impl Drop for ActiveDomEventGuard {
  fn drop(&mut self) {
    let event_id = self.event_id;
    ACTIVE_EVENTS.with(|events| {
      events.borrow_mut().remove(&event_id);
    });
  }
}

/// [`web_events::EventListenerInvoker`] that calls `addEventListener` callbacks registered inside a
/// [`WindowRealm`].
///
/// This is used by host-driven DOM event dispatch (e.g. UI click handling) so that Rust events
/// dispatched via [`web_events::dispatch_event`] can invoke JS listeners.
///
/// Note: Unlike JS-driven `dispatchEvent(e)`, this adapter synthesizes a JS `Event` object for each
/// listener invocation. This is sufficient for `preventDefault()`/propagation control (which
/// mutate the shared Rust [`web_events::Event`]) and keeps the invoker independent from any
/// long-lived `vm-js` scope borrows.
pub(crate) struct WindowRealmDomEventListenerInvoker {
  /// Pointer to the owning executor's `Option<WindowRealm>` slot.
  ///
  /// This is used so Rust-driven DOM event dispatch can invoke JS listeners without requiring the
  /// caller to thread `&mut WindowRealm` through `web_events::dispatch_event`.
  realm: *mut Option<WindowRealm>,
}

impl WindowRealmDomEventListenerInvoker {
  pub(crate) fn new(realm: *mut Option<WindowRealm>) -> Self {
    Self { realm }
  }

  fn js_value_for_target(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    window_obj: GcObject,
    document_obj: GcObject,
    target: Option<web_events::EventTargetId>,
  ) -> Result<Value, VmError> {
    match target {
      None => Ok(Value::Null),
      Some(web_events::EventTargetId::Window) => Ok(Value::Object(window_obj)),
      Some(web_events::EventTargetId::Document) => Ok(Value::Object(document_obj)),
      Some(web_events::EventTargetId::Node(node_id)) => {
        get_or_create_node_wrapper(vm, scope, document_obj, node_id)
      }
      Some(web_events::EventTargetId::Opaque(_)) => Ok(Value::Null),
    }
  }

  fn alloc_js_event_object(
    scope: &mut Scope<'_>,
    document_obj: GcObject,
    event: &web_events::Event,
  ) -> Result<GcObject, VmError> {
    let proto_key_name = if event.detail.is_some() {
      CUSTOM_EVENT_PROTOTYPE_KEY
    } else {
      EVENT_PROTOTYPE_KEY
    };
    let proto_key = alloc_key(scope, proto_key_name)?;
    let proto = match scope
      .heap()
      .object_get_own_data_property_value(document_obj, &proto_key)?
    {
      Some(Value::Object(obj)) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "document is missing required Event prototype",
        ))
      }
    };

    let event_obj = scope.alloc_object()?;
    scope
      .heap_mut()
      .object_set_prototype(event_obj, Some(proto))?;

    // Base event fields (immutable for the lifetime of this dispatch).
    let type_key = alloc_key(scope, "type")?;
    let type_s = scope.alloc_string(&event.type_)?;
    scope.push_root(Value::String(type_s))?;
    scope.define_property(event_obj, type_key, data_desc(Value::String(type_s)))?;

    let bubbles_key = alloc_key(scope, "bubbles")?;
    scope.define_property(event_obj, bubbles_key, data_desc(Value::Bool(event.bubbles)))?;

    let cancelable_key = alloc_key(scope, "cancelable")?;
    scope.define_property(
      event_obj,
      cancelable_key,
      data_desc(Value::Bool(event.cancelable)),
    )?;

    let composed_key = alloc_key(scope, "composed")?;
    scope.define_property(event_obj, composed_key, data_desc(Value::Bool(event.composed)))?;

    if let Some(detail) = event.detail {
      let detail_key = alloc_key(scope, "detail")?;
      scope.define_property(event_obj, detail_key, data_desc(detail))?;
    }

    Ok(event_obj)
  }

  fn sync_event_object(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    window_obj: GcObject,
    document_obj: GcObject,
    event_obj: GcObject,
    event: &web_events::Event,
  ) -> Result<(), VmError> {
    scope.push_root(Value::Object(event_obj))?;

    let target_key = alloc_key(scope, "target")?;
    let target_v = Self::js_value_for_target(vm, scope, window_obj, document_obj, event.target)?;
    scope.define_property(event_obj, target_key, data_desc(target_v))?;

    let current_target_key = alloc_key(scope, "currentTarget")?;
    let current_target_v =
      Self::js_value_for_target(vm, scope, window_obj, document_obj, event.current_target)?;
    scope.define_property(event_obj, current_target_key, data_desc(current_target_v))?;

    let event_phase_key = alloc_key(scope, "eventPhase")?;
    scope.define_property(
      event_obj,
      event_phase_key,
      data_desc(Value::Number(event.event_phase_numeric() as f64)),
    )?;

    let time_stamp_key = alloc_key(scope, "timeStamp")?;
    scope.define_property(
      event_obj,
      time_stamp_key,
      data_desc(Value::Number(event.time_stamp)),
    )?;

    let is_trusted_key = alloc_key(scope, "isTrusted")?;
    scope.define_property(
      event_obj,
      is_trusted_key,
      data_desc(Value::Bool(event.is_trusted)),
    )?;

    let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
    scope.define_property(
      event_obj,
      default_prevented_key,
      data_desc(Value::Bool(event.default_prevented)),
    )?;

    let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
    scope.define_property(
      event_obj,
      cancel_bubble_key,
      data_desc(Value::Bool(event.propagation_stopped)),
    )?;

    Ok(())
  }
}

impl web_events::EventListenerInvoker for WindowRealmDomEventListenerInvoker {
  fn invoke(
    &mut self,
    listener_id: web_events::ListenerId,
    event: &mut web_events::Event,
  ) -> std::result::Result<(), web_events::DomError> {
    // SAFETY: `BrowserTabHost` stores the returned invoker alongside the owning executor, so the
    // pointer remains valid for the lifetime of the host. Dispatch is single-threaded and
    // non-reentrant with respect to other `WindowRealm` borrows.
    let Some(realm) = unsafe { &mut *self.realm }.as_mut() else {
      // No JS realm to invoke listeners in.
      return Ok(());
    };

    let realm_id = realm.realm_id;
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
      realm: realm_id,
      script_or_module: None,
    });

    let window_obj = realm_ref.global_object();
    let document_obj = {
      scope
        .push_root(Value::Object(window_obj))
        .map_err(|e| web_events::DomError::new(e.to_string()))?;
      let document_key =
        alloc_key(&mut scope, "document").map_err(|e| web_events::DomError::new(e.to_string()))?;
      match vm
        .get(&mut scope, window_obj, document_key)
        .map_err(|e| web_events::DomError::new(e.to_string()))?
      {
        Value::Object(obj) => obj,
        _ => return Ok(()),
      }
    };

    let listener_roots = get_or_create_event_listener_roots(&mut scope, document_obj)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
    let listener_key = listener_id_property_key(&mut scope, listener_id)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
    let Some(callback) = scope
      .heap()
      .object_get_own_data_property_value(listener_roots, &listener_key)
      .map_err(|e| web_events::DomError::new(e.to_string()))?
    else {
      // Callback root missing; treat as no-op.
      return Ok(());
    };

    let event_obj = Self::alloc_js_event_object(&mut scope, document_obj, event)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
    scope
      .push_root(Value::Object(event_obj))
      .map_err(|e| web_events::DomError::new(e.to_string()))?;

    Self::sync_event_object(&mut vm, &mut scope, window_obj, document_obj, event_obj, event)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;

    let event_id = NEXT_ACTIVE_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    ACTIVE_EVENTS.with(|events| {
      events
        .borrow_mut()
        .insert(event_id, NonNull::from(&mut *event));
    });
    let _active_guard = ActiveDomEventGuard { event_id };
    let event_id_key =
      alloc_key(&mut scope, EVENT_ID_KEY).map_err(|e| web_events::DomError::new(e.to_string()))?;
    scope
      .define_property(
        event_obj,
        event_id_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Number(event_id as f64),
            writable: true,
          },
        },
      )
      .map_err(|e| web_events::DomError::new(e.to_string()))?;

    let current_target = match event.current_target {
      Some(t) => Self::js_value_for_target(
        &mut vm,
        &mut scope,
        window_obj,
        document_obj,
        Some(t),
      )
      .map_err(|e| web_events::DomError::new(e.to_string()))?,
      None => Value::Undefined,
    };

    // Invoke callback, swallowing exceptions to match web platform behavior.
    let mut host_ctx = ();
    let call_result: Result<(), VmError> = (|| {
      let event_value = Value::Object(event_obj);
      if scope.heap().is_callable(callback)? {
        vm.call(&mut host_ctx, &mut scope, callback, current_target, &[event_value])?;
        Ok(())
      } else if let Value::Object(callback_obj) = callback {
        let handle_event_key = alloc_key(&mut scope, "handleEvent")?;
        let handle_event = vm.get(&mut scope, callback_obj, handle_event_key)?;
        if !scope.heap().is_callable(handle_event)? {
          return Err(VmError::TypeError(
            "EventTarget listener callback has no callable handleEvent",
          ));
        }
        vm.call(&mut host_ctx, &mut scope, handle_event, callback, &[event_value])?;
        Ok(())
      } else {
        Err(VmError::TypeError(
          "EventTarget listener is not callable and not an object",
        ))
      }
    })();
    if let Err(_err) = call_result {
      // Listener errors should not abort dispatch.
    }

    // Best-effort cleanup: remove callback roots for `{ once: true }` listeners.
    if let Ok(Some(dom_source_id)) = dom_source_id_from_document(&mut scope, document_obj) {
      if let Some(dom_ptr) = dom_for_source(dom_source_id) {
        // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid
        // for the lifetime of the associated host document.
        let dom = unsafe { dom_ptr.as_ref() };
        let _ =
          remove_listener_root_if_unused(&mut scope, document_obj, dom.events(), listener_id, None);
      }
    }

    Ok(())
  }
}

struct VmJsDomEventInvoker<'a, 'host, 'hooks> {
  vm: *mut Vm,
  scope: *mut Scope<'a>,
  host: *mut (dyn VmHost + 'host),
  hooks: *mut (dyn VmHostHooks + 'hooks),
  window_obj: GcObject,
  document_obj: GcObject,
  event_obj: GcObject,
  listener_roots_owner: GcObject,
  listener_roots: GcObject,
  opaque_target_obj: Option<GcObject>,
  registry: *const web_events::EventListenerRegistry,
}

impl<'a, 'host, 'hooks> VmJsDomEventInvoker<'a, 'host, 'hooks> {
  fn js_value_for_target(
    &mut self,
    target: Option<web_events::EventTargetId>,
  ) -> Result<Value, VmError> {
    let scope = unsafe { &mut *self.scope };
    match target {
      None => Ok(Value::Null),
      Some(web_events::EventTargetId::Window) => Ok(Value::Object(self.window_obj)),
      Some(web_events::EventTargetId::Document) => Ok(Value::Object(self.document_obj)),
      Some(web_events::EventTargetId::Node(node_id)) => {
        let vm = unsafe { &mut *self.vm };
        get_or_create_node_wrapper(vm, scope, self.document_obj, node_id)
      }
      Some(web_events::EventTargetId::Opaque(_)) => Ok(match self.opaque_target_obj {
        Some(obj) => Value::Object(obj),
        None => Value::Null,
      }),
    }
  }

  fn sync_event_object(&mut self, event: &web_events::Event) -> Result<(), VmError> {
    let scope = unsafe { &mut *self.scope };
    scope.push_root(Value::Object(self.event_obj))?;

    let target_key = alloc_key(scope, "target")?;
    let target_v = self.js_value_for_target(event.target)?;
    scope.define_property(self.event_obj, target_key, data_desc(target_v))?;

    let current_target_key = alloc_key(scope, "currentTarget")?;
    let current_target_v = self.js_value_for_target(event.current_target)?;
    scope.define_property(self.event_obj, current_target_key, data_desc(current_target_v))?;

    let event_phase_key = alloc_key(scope, "eventPhase")?;
    scope.define_property(
      self.event_obj,
      event_phase_key,
      data_desc(Value::Number(event.event_phase_numeric() as f64)),
    )?;

    let time_stamp_key = alloc_key(scope, "timeStamp")?;
    scope.define_property(
      self.event_obj,
      time_stamp_key,
      data_desc(Value::Number(event.time_stamp)),
    )?;

    let is_trusted_key = alloc_key(scope, "isTrusted")?;
    scope.define_property(
      self.event_obj,
      is_trusted_key,
      data_desc(Value::Bool(event.is_trusted)),
    )?;

    let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
    scope.define_property(
      self.event_obj,
      default_prevented_key,
      data_desc(Value::Bool(event.default_prevented)),
    )?;

    let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
    scope.define_property(
      self.event_obj,
      cancel_bubble_key,
      data_desc(Value::Bool(event.propagation_stopped)),
    )?;

    if let Some(detail) = event.detail {
      let detail_key = alloc_key(scope, "detail")?;
      scope.define_property(self.event_obj, detail_key, data_desc(detail))?;
    }

    Ok(())
  }

  fn report_listener_exception(&mut self, err: VmError) {
    let scope = unsafe { &mut *self.scope };
    let vm = unsafe { &mut *self.vm };
    let host = unsafe { &mut *self.host };
    let hooks = unsafe { &mut *self.hooks };
    let message = crate::js::vm_error_format::vm_error_to_string(scope.heap_mut(), err);

    let console_key = match alloc_key(scope, "console") {
      Ok(key) => key,
      Err(_) => return,
    };
    let Some(Value::Object(console_obj)) = scope
      .heap()
      .object_get_own_data_property_value(self.window_obj, &console_key)
      .ok()
      .flatten()
    else {
      return;
    };

    let error_key = match alloc_key(scope, "error") {
      Ok(key) => key,
      Err(_) => return,
    };
    let Some(func) = scope
      .heap()
      .object_get_own_data_property_value(console_obj, &error_key)
      .ok()
      .flatten()
    else {
      return;
    };
    if !matches!(func, Value::Object(_)) {
      return;
    }
    if scope.heap().is_callable(func).ok() != Some(true) {
      return;
    }

    let msg_s = match scope.alloc_string(&message) {
      Ok(s) => s,
      Err(_) => return,
    };
    let _ = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      func,
      Value::Object(console_obj),
      &[Value::String(msg_s)],
    );
  }
}

impl web_events::EventListenerInvoker for VmJsDomEventInvoker<'_, '_, '_> {
  fn invoke(
    &mut self,
    listener_id: web_events::ListenerId,
    event: &mut web_events::Event,
  ) -> std::result::Result<(), web_events::DomError> {
    let scope = unsafe { &mut *self.scope };
    let vm = unsafe { &mut *self.vm };
    let host = unsafe { &mut *self.host };
    let hooks = unsafe { &mut *self.hooks };
    let registry = unsafe { &*self.registry };

    // Look up the registered callback function/object. If it is missing, treat it as a no-op.
    let listener_key = listener_id_property_key(scope, listener_id)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
    let Some(callback) = scope
      .heap()
      .object_get_own_data_property_value(self.listener_roots, &listener_key)
      .map_err(|e| web_events::DomError::new(e.to_string()))?
    else {
      return Ok(());
    };

    // Update JS-visible event fields for this invocation.
    self
      .sync_event_object(event)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;

    // Install the active Rust `Event` pointer so Event.prototype methods can mutate it.
    let event_id = NEXT_ACTIVE_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    ACTIVE_EVENTS.with(|events| {
      events
        .borrow_mut()
        .insert(event_id, NonNull::from(&mut *event));
    });
    let _active_guard = ActiveDomEventGuard { event_id };

    let event_id_key = alloc_key(scope, EVENT_ID_KEY)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
    scope
      .define_property(
        self.event_obj,
        event_id_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Number(event_id as f64),
            writable: true,
          },
        },
      )
      .map_err(|e| web_events::DomError::new(e.to_string()))?;

    let current_target = match event.current_target {
      Some(t) => self
        .js_value_for_target(Some(t))
        .map_err(|e| web_events::DomError::new(e.to_string()))?,
      None => Value::Undefined,
    };

    // DOM dispatch uses WebIDL's "call a user object's operation" algorithm for EventListener:
    // - If the listener is callable (function), invoke it with `this = currentTarget`.
    // - Otherwise, invoke `callback.handleEvent(event)` with `this = callback`.
    let call_result = (|| -> Result<(), VmError> {
      let event_value = Value::Object(self.event_obj);
      if scope.heap().is_callable(callback)? {
        vm.call_with_host_and_hooks(host, scope, hooks, callback, current_target, &[event_value])?;
        Ok(())
      } else if let Value::Object(callback_obj) = callback {
        let handle_event_key = alloc_key(scope, "handleEvent")?;
        let handle_event = vm.get(scope, callback_obj, handle_event_key)?;
        if !scope.heap().is_callable(handle_event)? {
          return Err(VmError::TypeError(
            "EventTarget listener callback has no callable handleEvent",
          ));
        }
        vm.call_with_host_and_hooks(host, scope, hooks, handle_event, callback, &[event_value])?;
        Ok(())
      } else {
        Err(VmError::TypeError(
          "EventTarget listener is not callable and not an object",
        ))
      }
    })();

    if let Err(err) = call_result {
      // Per web platform behavior, exceptions from event listeners should not abort `dispatchEvent`.
      self.report_listener_exception(err);
    }

    // `dispatch_event` can remove listeners during dispatch (`{ once: true }`). Drop the callback
    // root if the listener ID is no longer referenced.
    let target_for_owner = match event.current_target {
      Some(t @ web_events::EventTargetId::Opaque(_)) => Some(t),
      _ => None,
    };
    if let Err(err) = remove_listener_root_if_unused(
      scope,
      self.listener_roots_owner,
      registry,
      listener_id,
      target_for_owner,
    ) {
      return Err(web_events::DomError::new(err.to_string()));
    }

    Ok(())
  }
}

fn rust_event_from_js_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  event_obj: GcObject,
) -> Result<web_events::Event, VmError> {
  let type_key = alloc_key(scope, "type")?;
  let type_value = vm.get(scope, event_obj, type_key)?;
  let type_string = scope.heap_mut().to_string(type_value)?;
  let type_name = scope
    .heap()
    .get_string(type_string)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let bubbles_key = alloc_key(scope, "bubbles")?;
  let bubbles = match scope
    .heap()
    .object_get_own_data_property_value(event_obj, &bubbles_key)?
  {
    Some(v) => scope.heap().to_boolean(v)?,
    None => false,
  };

  let cancelable_key = alloc_key(scope, "cancelable")?;
  let cancelable = match scope
    .heap()
    .object_get_own_data_property_value(event_obj, &cancelable_key)?
  {
    Some(v) => scope.heap().to_boolean(v)?,
    None => false,
  };

  let composed_key = alloc_key(scope, "composed")?;
  let composed = match scope
    .heap()
    .object_get_own_data_property_value(event_obj, &composed_key)?
  {
    Some(v) => scope.heap().to_boolean(v)?,
    None => false,
  };

  let detail_key = alloc_key(scope, "detail")?;
  let detail = scope
    .heap()
    .object_get_own_data_property_value(event_obj, &detail_key)?;

  let mut event = if let Some(detail) = detail {
    web_events::Event::new_custom_event(
      type_name,
      web_events::CustomEventInit {
        bubbles,
        cancelable,
        composed,
        detail: if matches!(detail, Value::Undefined) {
          Value::Null
        } else {
          detail
        },
      },
    )
  } else {
    web_events::Event::new(
      type_name,
      web_events::EventInit {
        bubbles,
        cancelable,
        composed,
      },
    )
  };

  // Preserve an already-canceled Event when re-dispatching.
  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  if let Some(v) = scope
    .heap()
    .object_get_own_data_property_value(event_obj, &default_prevented_key)?
  {
    event.default_prevented = scope.heap().to_boolean(v)?;
  }

  Ok(event)
}

fn event_target_add_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target_obj = event_target_resolve_this(scope, callee, this)?;
  let resolved = resolve_event_target(vm, scope, callee, target_obj)?;
  let ResolvedEventTarget {
    resolved,
    mut dom_ptr,
    listener_roots_owner,
    ..
  } = resolved;

  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_string = scope.heap_mut().to_string(type_arg)?;
  let type_name = scope
    .heap()
    .get_string(type_string)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let callback = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::Object(callback_obj) = callback else {
    // Per WebIDL/DOM, `null` callbacks are no-ops.
    return Ok(Value::Undefined);
  };

  let options_value = args.get(2).copied().unwrap_or(Value::Undefined);
  let options = parse_add_event_listener_options(scope, options_value)?;
  let listener_id = web_events::ListenerId::from_gc_object(callback_obj);

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  dom
    .events_mut()
    .add_event_listener(resolved.target_id, &type_name, listener_id, options);

  // Root the callback while it's registered so it survives GC.
  let roots = get_or_create_event_listener_roots(scope, listener_roots_owner)?;
  let listener_key = listener_id_property_key(scope, listener_id)?;
  scope.push_root(callback)?;
  scope.define_property(roots, listener_key, data_desc(callback))?;

  Ok(Value::Undefined)
}

fn event_target_remove_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target_obj = event_target_resolve_this(scope, callee, this)?;
  let resolved = resolve_event_target(vm, scope, callee, target_obj)?;
  let ResolvedEventTarget {
    resolved,
    mut dom_ptr,
    listener_roots_owner,
    ..
  } = resolved;

  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_string = scope.heap_mut().to_string(type_arg)?;
  let type_name = scope
    .heap()
    .get_string(type_string)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let callback = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::Object(callback_obj) = callback else {
    return Ok(Value::Undefined);
  };

  let options_value = args.get(2).copied().unwrap_or(Value::Undefined);
  let capture = parse_event_listener_capture(scope, options_value)?;
  let listener_id = web_events::ListenerId::from_gc_object(callback_obj);

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let removed = dom
    .events_mut()
    .remove_event_listener(resolved.target_id, &type_name, listener_id, capture);
  if removed {
    let target_for_owner = match resolved.target_id {
      web_events::EventTargetId::Opaque(_) => Some(resolved.target_id),
      _ => None,
    };
    remove_listener_root_if_unused(
      scope,
      listener_roots_owner,
      dom.events(),
      listener_id,
      target_for_owner,
    )?;
  }

  Ok(Value::Undefined)
}

fn event_target_dispatch_event_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target_obj = event_target_resolve_this(scope, callee, this)?;
  let resolved = resolve_event_target(vm, scope, callee, target_obj)?;
  let ResolvedEventTarget {
    resolved,
    dom_ptr,
    listener_roots_owner,
    opaque_target_obj,
  } = resolved;

  let event_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(event_obj) = event_value else {
    return Err(VmError::TypeError(
      "EventTarget.dispatchEvent: event is not an object",
    ));
  };
  scope.push_root(Value::Object(event_obj))?;

  let mut rust_event = rust_event_from_js_event(vm, scope, event_obj)?;

  // Ensure base Event fields are observable even if there are no listeners.
  {
    let target_key = alloc_key(scope, "target")?;
    let target_v = match resolved.target_id {
      web_events::EventTargetId::Window => Value::Object(resolved.window_obj),
      web_events::EventTargetId::Document => Value::Object(resolved.document_obj),
      web_events::EventTargetId::Node(node_id) => {
        get_or_create_node_wrapper(vm, scope, resolved.document_obj, node_id)?
      }
      web_events::EventTargetId::Opaque(_) => Value::Object(
        opaque_target_obj.ok_or_else(|| {
          VmError::InvariantViolation("opaque EventTarget is missing required JS object handle")
        })?,
      ),
    };
    scope.define_property(event_obj, target_key, data_desc(target_v))?;

    let current_target_key = alloc_key(scope, "currentTarget")?;
    scope.define_property(event_obj, current_target_key, data_desc(Value::Null))?;

    let event_phase_key = alloc_key(scope, "eventPhase")?;
    scope.define_property(event_obj, event_phase_key, data_desc(Value::Number(0.0)))?;
  }

  // Reset per-dispatch propagation flags on the JS-visible object.
  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(event_obj, cancel_bubble_key, data_desc(Value::Bool(false)))?;

  let roots = get_or_create_event_listener_roots(scope, listener_roots_owner)?;
  let mut invoker = VmJsDomEventInvoker {
    vm,
    scope,
    host,
    hooks,
    window_obj: resolved.window_obj,
    document_obj: resolved.document_obj,
    event_obj,
    listener_roots_owner,
    listener_roots: roots,
    opaque_target_obj,
    registry: unsafe { dom_ptr.as_ref() }.events(),
  };

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let result = web_events::dispatch_event(
    resolved.target_id,
    &mut rust_event,
    dom,
    dom.events(),
    &mut invoker,
  )
  .map_err(|_err| VmError::TypeError("EventTarget.dispatchEvent failed"))?;

  // Persist final per-dispatch state.
  {
    let current_target_key = alloc_key(scope, "currentTarget")?;
    scope.define_property(event_obj, current_target_key, data_desc(Value::Null))?;
    let event_phase_key = alloc_key(scope, "eventPhase")?;
    scope.define_property(event_obj, event_phase_key, data_desc(Value::Number(0.0)))?;
    let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
    scope.define_property(
      event_obj,
      default_prevented_key,
      data_desc(Value::Bool(rust_event.default_prevented)),
    )?;
  }

  Ok(Value::Bool(result))
}

fn document_create_event_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Err(VmError::TypeError(
      "document.createEvent must be called on a document object",
    ));
  };

  let interface_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let interface_string = scope.heap_mut().to_string(interface_arg)?;
  let interface_name = scope
    .heap()
    .get_string(interface_string)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();
  let name = interface_name.trim();

  enum Kind {
    Event,
    CustomEvent,
  }

  let kind = if name.eq_ignore_ascii_case("Event") {
    Kind::Event
  } else if name.eq_ignore_ascii_case("CustomEvent") {
    Kind::CustomEvent
  } else {
    return Err(VmError::Throw(make_dom_exception(
      scope,
      "NotSupportedError",
      &format!("Unsupported event interface: {name}"),
    )?));
  };

  let proto_key = match kind {
    Kind::Event => EVENT_PROTOTYPE_KEY,
    Kind::CustomEvent => CUSTOM_EVENT_PROTOTYPE_KEY,
  };
  let proto_key = alloc_key(scope, proto_key)?;
  let proto = scope
    .heap()
    .object_get_own_data_property_value(document_obj, &proto_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  if let Some(proto) = proto {
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  }

  let empty = scope.alloc_string("")?;

  let type_key = alloc_key(scope, "type")?;
  scope.define_property(obj, type_key, data_desc(Value::String(empty)))?;

  let bubbles_key = alloc_key(scope, "bubbles")?;
  scope.define_property(obj, bubbles_key, data_desc(Value::Bool(false)))?;

  let cancelable_key = alloc_key(scope, "cancelable")?;
  scope.define_property(obj, cancelable_key, data_desc(Value::Bool(false)))?;

  let composed_key = alloc_key(scope, "composed")?;
  scope.define_property(obj, composed_key, data_desc(Value::Bool(false)))?;

  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  scope.define_property(obj, default_prevented_key, data_desc(Value::Bool(false)))?;

  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(obj, cancel_bubble_key, data_desc(Value::Bool(false)))?;

  if matches!(kind, Kind::CustomEvent) {
    let detail_key = alloc_key(scope, "detail")?;
    scope.define_property(obj, detail_key, data_desc(Value::Null))?;
  }

  Ok(Value::Object(obj))
}

fn node_append_child_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let base_url = current_base_url_for_dynamic_scripts(vm);
  let Value::Object(parent_obj) = this else {
    return Err(VmError::TypeError(
      "Node.appendChild must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(parent_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.appendChild requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let parent_index = match scope
    .heap()
    .object_get_own_data_property_value(parent_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.appendChild must be called on a node object",
      ));
    }
  };

  let child_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(child_obj) = child_value else {
    return Err(VmError::TypeError(
      "Node.appendChild requires a node argument",
    ));
  };

  let child_source_id = match scope
    .heap()
    .object_get_own_data_property_value(child_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.appendChild requires a node argument",
      ));
    }
  };
  if child_source_id != source_id {
    return Err(VmError::TypeError(
      "Node.appendChild cannot move nodes between documents",
    ));
  }

  let child_index = match scope
    .heap()
    .object_get_own_data_property_value(child_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.appendChild requires a node argument",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Node.appendChild requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let (parent_node_id, child_node_id, old_parent, child_is_fragment, fragment_children) = {
    let dom = unsafe { dom_ptr.as_mut() };

    let parent_node_id = dom
      .node_id_from_index(parent_index)
      .map_err(|_| VmError::TypeError("Node.appendChild must be called on a node object"))?;
    let child_node_id = dom
      .node_id_from_index(child_index)
      .map_err(|_| VmError::TypeError("Node.appendChild requires a node argument"))?;

    let old_parent = dom.parent_node(child_node_id);
    let child_is_fragment = matches!(&dom.node(child_node_id).kind, NodeKind::DocumentFragment);
    let fragment_children = if child_is_fragment {
      dom.node(child_node_id).children.clone()
    } else {
      Vec::new()
    };

    if let Err(err) = dom.append_child(parent_node_id, child_node_id) {
      return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
    }

    (
      parent_node_id,
      child_node_id,
      old_parent,
      child_is_fragment,
      fragment_children,
    )
  };

  let document_obj = node_wrapper_document_obj(scope, parent_obj, parent_node_id)?;
  sync_cached_child_nodes_for_wrapper(vm, scope, source_id, document_obj, parent_obj, parent_node_id)?;
  if let Some(old_parent) = old_parent {
    if old_parent != parent_node_id {
      sync_cached_child_nodes_for_node_id(vm, scope, source_id, document_obj, old_parent)?;
    }
  }
  if child_is_fragment {
    sync_cached_child_nodes_for_wrapper(
      vm,
      scope,
      source_id,
      document_obj,
      child_obj,
      child_node_id,
    )?;
  }

  // Dynamic `<script>` preparation: run after insertion so nodes are connected.
  {
    // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
    // lifetime of the associated host document.
    let dom = unsafe { dom_ptr.as_mut() };
    if child_is_fragment {
      for node in fragment_children {
        prepare_dynamic_scripts_on_subtree_insertion(dom, node, &base_url)?;
      }
    } else {
      prepare_dynamic_scripts_on_subtree_insertion(dom, child_node_id, &base_url)?;
    }
    if is_html_script_element(dom, parent_node_id) {
      prepare_dynamic_script(dom, parent_node_id, &base_url)?;
    }
  }

  Ok(child_value)
}

fn node_insert_before_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let base_url = current_base_url_for_dynamic_scripts(vm);
  let Value::Object(parent_obj) = this else {
    return Err(VmError::TypeError(
      "Node.insertBefore must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(parent_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.insertBefore requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let parent_index = match scope
    .heap()
    .object_get_own_data_property_value(parent_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.insertBefore must be called on a node object",
      ));
    }
  };

  let new_child_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(new_child_obj) = new_child_value else {
    return Err(VmError::TypeError(
      "Node.insertBefore requires a node argument",
    ));
  };

  let new_child_source_id = match scope
    .heap()
    .object_get_own_data_property_value(new_child_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.insertBefore requires a node argument",
      ));
    }
  };
  if new_child_source_id != source_id {
    return Err(VmError::TypeError(
      "Node.insertBefore cannot move nodes between documents",
    ));
  }

  let new_child_index = match scope
    .heap()
    .object_get_own_data_property_value(new_child_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.insertBefore requires a node argument",
      ));
    }
  };

  let reference_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let reference_index = if matches!(reference_value, Value::Null | Value::Undefined) {
    None
  } else {
    let Value::Object(reference_obj) = reference_value else {
      return Err(VmError::TypeError(
        "Node.insertBefore requires a reference node argument",
      ));
    };

    let reference_source_id = match scope
      .heap()
      .object_get_own_data_property_value(reference_obj, &source_id_key)?
    {
      Some(Value::Number(n)) => n as u64,
      _ => {
        return Err(VmError::TypeError(
          "Node.insertBefore requires a reference node argument",
        ));
      }
    };
    if reference_source_id != source_id {
      return Err(VmError::TypeError(
        "Node.insertBefore cannot move nodes between documents",
      ));
    }

    match scope
      .heap()
      .object_get_own_data_property_value(reference_obj, &node_id_key)?
    {
      Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => Some(n as usize),
      _ => {
        return Err(VmError::TypeError(
          "Node.insertBefore requires a reference node argument",
        ));
      }
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Node.insertBefore requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let (parent_node_id, new_child_node_id, old_parent, new_child_is_fragment, fragment_children) = {
    let dom = unsafe { dom_ptr.as_mut() };

    let parent_node_id = dom
      .node_id_from_index(parent_index)
      .map_err(|_| VmError::TypeError("Node.insertBefore must be called on a node object"))?;
    let new_child_node_id = dom
      .node_id_from_index(new_child_index)
      .map_err(|_| VmError::TypeError("Node.insertBefore requires a node argument"))?;
    let reference_node_id = match reference_index {
      Some(reference_index) => Some(
        dom
          .node_id_from_index(reference_index)
          .map_err(|_| VmError::TypeError("Node.insertBefore requires a reference node argument"))?,
      ),
      None => None,
    };

    let old_parent = dom.parent_node(new_child_node_id);
    let new_child_is_fragment = matches!(&dom.node(new_child_node_id).kind, NodeKind::DocumentFragment);
    let fragment_children = if new_child_is_fragment {
      dom.node(new_child_node_id).children.clone()
    } else {
      Vec::new()
    };

    if let Err(err) = dom.insert_before(parent_node_id, new_child_node_id, reference_node_id) {
      return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
    }

    (
      parent_node_id,
      new_child_node_id,
      old_parent,
      new_child_is_fragment,
      fragment_children,
    )
  };

  let document_obj = node_wrapper_document_obj(scope, parent_obj, parent_node_id)?;
  sync_cached_child_nodes_for_wrapper(vm, scope, source_id, document_obj, parent_obj, parent_node_id)?;
  if let Some(old_parent) = old_parent {
    if old_parent != parent_node_id {
      sync_cached_child_nodes_for_node_id(vm, scope, source_id, document_obj, old_parent)?;
    }
  }
  if new_child_is_fragment {
    sync_cached_child_nodes_for_wrapper(
      vm,
      scope,
      source_id,
      document_obj,
      new_child_obj,
      new_child_node_id,
    )?;
  }

  // Dynamic `<script>` preparation: run after insertion so nodes are connected.
  {
    // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
    // lifetime of the associated host document.
    let dom = unsafe { dom_ptr.as_mut() };
    if new_child_is_fragment {
      for node in fragment_children {
        prepare_dynamic_scripts_on_subtree_insertion(dom, node, &base_url)?;
      }
    } else {
      prepare_dynamic_scripts_on_subtree_insertion(dom, new_child_node_id, &base_url)?;
    }
    if is_html_script_element(dom, parent_node_id) {
      prepare_dynamic_script(dom, parent_node_id, &base_url)?;
    }
  }

  Ok(new_child_value)
}

fn node_remove_child_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let base_url = current_base_url_for_dynamic_scripts(vm);
  let Value::Object(parent_obj) = this else {
    return Err(VmError::TypeError(
      "Node.removeChild must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(parent_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.removeChild requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let parent_index = match scope
    .heap()
    .object_get_own_data_property_value(parent_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.removeChild must be called on a node object",
      ));
    }
  };

  let child_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(child_obj) = child_value else {
    return Err(VmError::TypeError(
      "Node.removeChild requires a node argument",
    ));
  };

  let child_source_id = match scope
    .heap()
    .object_get_own_data_property_value(child_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.removeChild requires a node argument",
      ));
    }
  };
  if child_source_id != source_id {
    return Err(VmError::TypeError(
      "Node.removeChild cannot remove nodes between documents",
    ));
  }

  let child_index = match scope
    .heap()
    .object_get_own_data_property_value(child_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.removeChild requires a node argument",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Node.removeChild requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let parent_node_id = {
    let dom = unsafe { dom_ptr.as_mut() };

    let parent_node_id = dom
      .node_id_from_index(parent_index)
      .map_err(|_| VmError::TypeError("Node.removeChild must be called on a node object"))?;
    let child_node_id = dom
      .node_id_from_index(child_index)
      .map_err(|_| VmError::TypeError("Node.removeChild requires a node argument"))?;

    if let Err(err) = dom.remove_child(parent_node_id, child_node_id) {
      return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
    }

    parent_node_id
  };

  let document_obj = node_wrapper_document_obj(scope, parent_obj, parent_node_id)?;
  sync_cached_child_nodes_for_wrapper(vm, scope, source_id, document_obj, parent_obj, parent_node_id)?;

  // Dynamic `<script>` children-changed preparation: removing child nodes from a `<script>` can
  // trigger execution when the element had not started yet.
  {
    // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
    // lifetime of the associated host document.
    let dom = unsafe { dom_ptr.as_mut() };
    if is_html_script_element(dom, parent_node_id) {
      prepare_dynamic_script(dom, parent_node_id, &base_url)?;
    }
  }

  Ok(child_value)
}

fn node_replace_child_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let base_url = current_base_url_for_dynamic_scripts(vm);
  let Value::Object(parent_obj) = this else {
    return Err(VmError::TypeError(
      "Node.replaceChild must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(parent_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.replaceChild requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let parent_index = match scope
    .heap()
    .object_get_own_data_property_value(parent_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.replaceChild must be called on a node object",
      ));
    }
  };

  let new_child_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(new_child_obj) = new_child_value else {
    return Err(VmError::TypeError(
      "Node.replaceChild requires a node argument",
    ));
  };

  let new_child_source_id = match scope
    .heap()
    .object_get_own_data_property_value(new_child_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.replaceChild requires a node argument",
      ));
    }
  };
  if new_child_source_id != source_id {
    return Err(VmError::TypeError(
      "Node.replaceChild cannot move nodes between documents",
    ));
  }

  let new_child_index = match scope
    .heap()
    .object_get_own_data_property_value(new_child_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.replaceChild requires a node argument",
      ));
    }
  };

  let old_child_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::Object(old_child_obj) = old_child_value else {
    return Err(VmError::TypeError(
      "Node.replaceChild requires a node argument",
    ));
  };

  let old_child_source_id = match scope
    .heap()
    .object_get_own_data_property_value(old_child_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.replaceChild requires a node argument",
      ));
    }
  };
  if old_child_source_id != source_id {
    return Err(VmError::TypeError(
      "Node.replaceChild cannot move nodes between documents",
    ));
  }

  let old_child_index = match scope
    .heap()
    .object_get_own_data_property_value(old_child_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.replaceChild requires a node argument",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Node.replaceChild requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let (parent_node_id, new_child_node_id, old_parent, new_child_is_fragment, fragment_children) = {
    let dom = unsafe { dom_ptr.as_mut() };

    let parent_node_id = dom
      .node_id_from_index(parent_index)
      .map_err(|_| VmError::TypeError("Node.replaceChild must be called on a node object"))?;
    let new_child_node_id = dom
      .node_id_from_index(new_child_index)
      .map_err(|_| VmError::TypeError("Node.replaceChild requires a node argument"))?;
    let old_child_node_id = dom
      .node_id_from_index(old_child_index)
      .map_err(|_| VmError::TypeError("Node.replaceChild requires a node argument"))?;

    let old_parent = dom.parent_node(new_child_node_id);
    let new_child_is_fragment = matches!(&dom.node(new_child_node_id).kind, NodeKind::DocumentFragment);
    let fragment_children = if new_child_is_fragment {
      dom.node(new_child_node_id).children.clone()
    } else {
      Vec::new()
    };

    if let Err(err) = dom.replace_child(parent_node_id, new_child_node_id, old_child_node_id) {
      return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
    }

    (
      parent_node_id,
      new_child_node_id,
      old_parent,
      new_child_is_fragment,
      fragment_children,
    )
  };

  let document_obj = node_wrapper_document_obj(scope, parent_obj, parent_node_id)?;
  sync_cached_child_nodes_for_wrapper(vm, scope, source_id, document_obj, parent_obj, parent_node_id)?;
  if let Some(old_parent) = old_parent {
    if old_parent != parent_node_id {
      sync_cached_child_nodes_for_node_id(vm, scope, source_id, document_obj, old_parent)?;
    }
  }
  if new_child_is_fragment {
    sync_cached_child_nodes_for_wrapper(
      vm,
      scope,
      source_id,
      document_obj,
      new_child_obj,
      new_child_node_id,
    )?;
  }

  // Dynamic `<script>` preparation: run after insertion so nodes are connected.
  {
    // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
    // lifetime of the associated host document.
    let dom = unsafe { dom_ptr.as_mut() };
    if new_child_is_fragment {
      for node in fragment_children {
        prepare_dynamic_scripts_on_subtree_insertion(dom, node, &base_url)?;
      }
    } else {
      prepare_dynamic_scripts_on_subtree_insertion(dom, new_child_node_id, &base_url)?;
    }
    if is_html_script_element(dom, parent_node_id) {
      prepare_dynamic_script(dom, parent_node_id, &base_url)?;
    }
  }

  Ok(old_child_value)
}

fn node_clone_node_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Node.cloneNode must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.cloneNode requires a DOM-backed node",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.cloneNode must be called on a node object",
      ));
    }
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Node.cloneNode requires a DOM-backed node",
      ));
    }
  };

  let deep_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let deep = scope.heap().to_boolean(deep_val)?;

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Node.cloneNode requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };

  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Node.cloneNode must be called on a node object"))?;

  let cloned = match dom.clone_node(node_id, deep) {
    Ok(cloned) => cloned,
    Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  };

  get_or_create_node_wrapper(vm, scope, document_obj, cloned)
}

fn node_traversal_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  this: Value,
  f: impl FnOnce(&dom2::Document, NodeId) -> Option<NodeId>,
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Null);
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Null),
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Null),
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => return Ok(Value::Null),
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Null);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Null),
  };

  match f(dom, node_id) {
    Some(found) => get_or_create_node_wrapper(vm, scope, document_obj, found),
    None => Ok(Value::Null),
  }
}

fn node_parent_node_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  node_traversal_getter(vm, scope, this, |dom, node| dom.parent_node(node))
}

fn node_first_child_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  node_traversal_getter(vm, scope, this, |dom, node| dom.first_child(node))
}

fn node_previous_sibling_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  node_traversal_getter(vm, scope, this, |dom, node| dom.previous_sibling(node))
}

fn node_next_sibling_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  node_traversal_getter(vm, scope, this, |dom, node| dom.next_sibling(node))
}

fn node_node_type_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, node_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let node_id = platform.require_node_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), node_id)
  };

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let node_type = match &dom.node(node_id).kind {
    NodeKind::Element { .. } | NodeKind::Slot { .. } => 1,
    NodeKind::Text { .. } => 3,
    NodeKind::ProcessingInstruction { .. } => 7,
    NodeKind::Comment { .. } => 8,
    NodeKind::Document { .. } => 9,
    NodeKind::Doctype { .. } => 10,
    NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => 11,
  };

  Ok(Value::Number(node_type as f64))
}

fn node_node_name_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, node_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let node_id = platform.require_node_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), node_id)
  };

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let name = match &dom.node(node_id).kind {
    NodeKind::Element { tag_name, .. } => tag_name.to_ascii_uppercase(),
    NodeKind::Slot { .. } => "SLOT".to_string(),
    NodeKind::Text { .. } => "#text".to_string(),
    NodeKind::ProcessingInstruction { target, .. } => target.to_string(),
    NodeKind::Comment { .. } => "#comment".to_string(),
    NodeKind::Document { .. } => "#document".to_string(),
    NodeKind::Doctype { name, .. } => name.to_string(),
    NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => "#document-fragment".to_string(),
  };

  Ok(Value::String(scope.alloc_string(&name)?))
}

fn node_owner_document_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let node_id = dom_platform_mut(vm)
    .ok_or(VmError::TypeError("Illegal invocation"))?
    .require_node_id(scope.heap(), Value::Object(wrapper_obj))?;
  if node_id.index() == 0 {
    return Ok(Value::Null);
  }

  let document_obj = node_wrapper_document_obj(scope, wrapper_obj, node_id)?;
  Ok(Value::Object(document_obj))
}

fn node_is_connected_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, node_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let node_id = platform.require_node_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), node_id)
  };

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  Ok(Value::Bool(dom.is_connected_for_scripting(node_id)))
}

fn node_parent_element_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, node_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let node_id = platform.require_node_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), node_id)
  };

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let Some(parent_id) = dom.parent_node(node_id) else {
    return Ok(Value::Null);
  };
  if !matches!(
    dom.node(parent_id).kind,
    NodeKind::Element { .. } | NodeKind::Slot { .. }
  ) {
    return Ok(Value::Null);
  }

  let document_obj = node_wrapper_document_obj(scope, wrapper_obj, node_id)?;
  get_or_create_node_wrapper(vm, scope, document_obj, parent_id)
}

fn node_last_child_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, node_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let node_id = platform.require_node_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), node_id)
  };

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let Some(child_id) = dom.last_child(node_id) else {
    return Ok(Value::Null);
  };
  let document_obj = node_wrapper_document_obj(scope, wrapper_obj, node_id)?;
  get_or_create_node_wrapper(vm, scope, document_obj, child_id)
}

fn node_has_child_nodes_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, node_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let node_id = platform.require_node_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), node_id)
  };

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  Ok(Value::Bool(dom.first_child(node_id).is_some()))
}

fn node_contains_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, node_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let node_id = platform.require_node_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), node_id)
  };

  let other_value = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(other_value, Value::Null | Value::Undefined) {
    return Ok(Value::Bool(false));
  }

  let other_id = dom_platform_mut(vm)
    .ok_or(VmError::TypeError("Illegal invocation"))?
    .require_node_id(scope.heap(), other_value)?;

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  Ok(Value::Bool(dom.ancestors(other_id).any(|ancestor| ancestor == node_id)))
}

fn node_child_nodes_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, node_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let node_id = platform.require_node_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), node_id)
  };

  let document_obj = node_wrapper_document_obj(scope, wrapper_obj, node_id)?;

  let child_nodes_key = alloc_key(scope, NODE_CHILD_NODES_KEY)?;
  let array = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &child_nodes_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      let array = scope.alloc_array(0)?;
      scope.push_root(Value::Object(array))?;
      if let Some(intrinsics) = vm.intrinsics() {
        scope
          .heap_mut()
          .object_set_prototype(array, Some(intrinsics.array_prototype()))?;
      }
      scope.define_property(
        wrapper_obj,
        child_nodes_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Object(array),
            writable: false,
          },
        },
      )?;
      array
    }
  };

  sync_child_nodes_array(vm, scope, dom_source_id, document_obj, node_id, array)?;
  Ok(Value::Object(array))
}

fn node_remove_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Node.remove must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Undefined),
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.remove must be called on a node object",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Undefined);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Node.remove must be called on a node object"))?;

  let Some(parent) = dom.parent_node(node_id) else {
    return Ok(Value::Undefined);
  };

  if let Err(err) = dom.remove_child(parent, node_id) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  // Keep cached `childNodes` live NodeLists updated.
  let document_obj = node_wrapper_document_obj(scope, wrapper_obj, node_id)?;
  sync_cached_child_nodes_for_node_id(vm, scope, source_id, document_obj, parent)?;

  Ok(Value::Undefined)
}

fn node_text_content_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Node.textContent must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.textContent requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.textContent must be called on a node object",
      ));
    }
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Node.textContent requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Node.textContent must be called on a node object"))?;

  match &dom.node(node_id).kind {
    NodeKind::Document { .. } | NodeKind::Doctype { .. } => Ok(Value::Null),
    NodeKind::Text { content } => Ok(Value::String(scope.alloc_string(content)?)),
    NodeKind::Comment { content } => Ok(Value::String(scope.alloc_string(content)?)),
    NodeKind::ProcessingInstruction { data, .. } => Ok(Value::String(scope.alloc_string(data)?)),
    NodeKind::Element { .. }
    | NodeKind::Slot { .. }
    | NodeKind::DocumentFragment
    | NodeKind::ShadowRoot { .. } => {
      let mut out = String::new();

      let mut remaining = dom.nodes_len().saturating_add(1);
      let mut stack: Vec<NodeId> = Vec::new();

      // Seed traversal with children in reverse so we pop in tree order.
      let root_node = dom.node(node_id);
      for &child in root_node.children.iter().rev() {
        if child.index() >= dom.nodes_len() {
          continue;
        }
        if dom.node(child).parent != Some(node_id) {
          continue;
        }
        // `ShadowRoot` is not part of the light DOM tree for `textContent` semantics.
        if matches!(&root_node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
          && matches!(&dom.node(child).kind, NodeKind::ShadowRoot { .. })
        {
          continue;
        }
        stack.push(child);
      }

      while let Some(id) = stack.pop() {
        if remaining == 0 {
          break;
        }
        remaining -= 1;

        let node = dom.node(id);
        if let NodeKind::Text { content } = &node.kind {
          out.push_str(content);
        }

        for &child in node.children.iter().rev() {
          if child.index() >= dom.nodes_len() {
            continue;
          }
          if dom.node(child).parent != Some(id) {
            continue;
          }
          if matches!(&node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
            && matches!(&dom.node(child).kind, NodeKind::ShadowRoot { .. })
          {
            continue;
          }
          stack.push(child);
        }
      }

      Ok(Value::String(scope.alloc_string(&out)?))
    }
  }
}

fn node_text_content_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let base_url = current_base_url_for_dynamic_scripts(vm);
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Node.textContent must be called on a node object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Node.textContent requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Node.textContent must be called on a node object",
      ));
    }
  };

  let value_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let value = match value_value {
    // `textContent` is `DOMString?`; `null` and `undefined` act as the empty string.
    Value::Null | Value::Undefined => String::new(),
    other => {
      let s = scope.heap_mut().to_string(other)?;
      scope
        .heap()
        .get_string(s)
        .map(|s| s.to_utf8_lossy())
        .unwrap_or_default()
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Node.textContent requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Node.textContent must be called on a node object"))?;

  #[derive(Clone, Copy)]
  enum TextContentTarget {
    Text,
    Comment,
    ProcessingInstruction,
    ReplaceChildren { preserve_shadow_roots: bool },
    NoOp,
  }

  let target = match &dom.node(node_id).kind {
    NodeKind::Text { .. } => TextContentTarget::Text,
    NodeKind::Comment { .. } => TextContentTarget::Comment,
    NodeKind::ProcessingInstruction { .. } => TextContentTarget::ProcessingInstruction,
    NodeKind::Element { .. } | NodeKind::Slot { .. } => {
      TextContentTarget::ReplaceChildren {
        preserve_shadow_roots: true,
      }
    }
    NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => TextContentTarget::ReplaceChildren {
      preserve_shadow_roots: false,
    },
    NodeKind::Document { .. } | NodeKind::Doctype { .. } => TextContentTarget::NoOp,
  };

  let mut maybe_script_children_changed: Option<NodeId> = None;
  match target {
    TextContentTarget::Text => {
      if let Err(err) = dom.set_text_data(node_id, &value) {
        return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
      }
      if let Some(parent) = dom.node(node_id).parent {
        if is_html_script_element(dom, parent) {
          maybe_script_children_changed = Some(parent);
        }
      }
    }
    TextContentTarget::Comment => {
      let node = dom.node_mut(node_id);
      if let NodeKind::Comment { content } = &mut node.kind {
        if content != &value {
          content.clear();
          content.push_str(&value);
        }
      }
    }
    TextContentTarget::ProcessingInstruction => {
      let node = dom.node_mut(node_id);
      if let NodeKind::ProcessingInstruction { data, .. } = &mut node.kind {
        if data != &value {
          data.clear();
          data.push_str(&value);
        }
      }
    }
    TextContentTarget::ReplaceChildren {
      preserve_shadow_roots,
    } => {
      let old_children = {
        let node = dom.node_mut(node_id);
        std::mem::take(&mut node.children)
      };

      let mut preserved: Vec<NodeId> = Vec::new();
      for child in old_children {
        if child.index() >= dom.nodes_len() {
          continue;
        }
        if dom.node(child).parent != Some(node_id) {
          continue;
        }

        if preserve_shadow_roots && matches!(&dom.node(child).kind, NodeKind::ShadowRoot { .. }) {
          preserved.push(child);
          continue;
        }

        dom.node_mut(child).parent = None;
      }

      dom.node_mut(node_id).children = preserved;

      if !value.is_empty() {
        let text_node = dom.create_text(&value);
        if let Err(err) = dom.append_child(node_id, text_node) {
          return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
        }
      }

      if is_html_script_element(dom, node_id) {
        maybe_script_children_changed = Some(node_id);
      }
    }
    TextContentTarget::NoOp => {}
  }

  if let Some(script) = maybe_script_children_changed {
    prepare_dynamic_script(dom, script, &base_url)?;
  }

  Ok(Value::Undefined)
}

fn text_data_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(text_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, text_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let text_id = platform.require_text_id(scope.heap(), Value::Object(text_obj))?;
    (platform.dom_source_id(), text_id)
  };

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let data = dom
    .text_data(text_id)
    .map_err(|_| VmError::TypeError("Illegal invocation"))?;
  Ok(Value::String(scope.alloc_string(data)?))
}

fn text_data_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let base_url = current_base_url_for_dynamic_scripts(vm);
  let Value::Object(text_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, text_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let text_id = platform.require_text_id(scope.heap(), Value::Object(text_obj))?;
    (platform.dom_source_id(), text_id)
  };

  let new_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_value = scope.heap_mut().to_string(new_value)?;
  let new_value = scope
    .heap()
    .get_string(new_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };

  if let Err(err) = dom.set_text_data(text_id, &new_value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  if let Some(parent) = dom.node(text_id).parent {
    if is_html_script_element(dom, parent) {
      prepare_dynamic_script(dom, parent, &base_url)?;
    }
  }

  Ok(Value::Undefined)
}

fn element_tag_name_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let (dom_source_id, node_id) = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let node_id = platform.require_element_id(scope.heap(), Value::Object(wrapper_obj))?;
    (platform.dom_source_id(), node_id)
  };

  let Some(dom_ptr) = dom_for_source(dom_source_id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  let tag = match &dom.node(node_id).kind {
    NodeKind::Element { tag_name, .. } => tag_name.as_str(),
    NodeKind::Slot { .. } => "slot",
    _ => return Err(VmError::TypeError("Illegal invocation")),
  };
  Ok(Value::String(scope.alloc_string(&tag.to_ascii_uppercase())?))
}

fn element_class_name_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Undefined);
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Undefined),
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Undefined),
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Undefined);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };

  let class_name = dom.element_class_name(node_id);
  let s = scope.alloc_string(class_name)?;
  Ok(Value::String(s))
}

fn element_class_name_set_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Undefined);
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Undefined),
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Undefined),
  };

  let new_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_value = scope.heap_mut().to_string(new_value)?;
  let new_value = scope
    .heap()
    .get_string(new_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Undefined);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };

  dom
    .set_element_class_name(node_id, &new_value)
    .map_err(|_| VmError::TypeError("failed to set Element.className"))?;

  Ok(Value::Undefined)
}

fn element_id_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Undefined);
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Undefined),
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Undefined),
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Undefined);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };

  let id = dom.element_id(node_id);
  let s = scope.alloc_string(id)?;
  Ok(Value::String(s))
}

fn element_id_set_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Undefined);
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Undefined),
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Undefined),
  };

  let new_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_value = scope.heap_mut().to_string(new_value)?;
  let new_value = scope
    .heap()
    .get_string(new_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::Undefined);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };

  dom
    .set_element_id(node_id, &new_value)
    .map_err(|_| VmError::TypeError("failed to set Element.id"))?;

  Ok(Value::Undefined)
}

fn native_slot_string(scope: &Scope<'_>, callee: GcObject) -> Result<String, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let slot = slots.get(0).copied().unwrap_or(Value::Undefined);
  let Value::String(s) = slot else {
    return Err(VmError::InvariantViolation(
      "expected native slot string argument",
    ));
  };
  Ok(scope.heap().get_string(s)?.to_utf8_lossy())
}

fn dom_node_id_from_obj(
  scope: &mut Scope<'_>,
  obj: GcObject,
) -> Result<Option<(NonNull<dom2::Document>, NodeId)>, VmError> {
  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(None),
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(None),
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(None);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(None),
  };

  Ok(Some((dom_ptr, node_id)))
}

fn element_reflected_string_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };

  let attr = native_slot_string(scope, callee)?;
  let Some((dom_ptr, node_id)) = dom_node_id_from_obj(scope, obj)? else {
    return Ok(Value::Undefined);
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let value = dom.get_attribute(node_id, &attr).ok().flatten().unwrap_or("");
  Ok(Value::String(scope.alloc_string(value)?))
}

fn element_reflected_string_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };

  let attr = native_slot_string(scope, callee)?;
  let Some((mut dom_ptr, node_id)) = dom_node_id_from_obj(scope, obj)? else {
    return Ok(Value::Undefined);
  };

  let new_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_value = scope.heap_mut().to_string(new_value)?;
  let new_value = scope
    .heap()
    .get_string(new_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  if let Err(err) = dom.set_attribute(node_id, &attr, &new_value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  if attr == "src" && is_html_script_element(dom, node_id) {
    let base_url = current_base_url_for_dynamic_scripts(vm);
    prepare_dynamic_script(dom, node_id, &base_url)?;
  }

  Ok(Value::Undefined)
}

fn element_reflected_bool_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };

  let attr = native_slot_string(scope, callee)?;
  let Some((dom_ptr, node_id)) = dom_node_id_from_obj(scope, obj)? else {
    return Ok(Value::Undefined);
  };

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  if attr == "async" && is_html_script_element(dom, node_id) {
    let force_async = dom.node(node_id).script_force_async;
    let async_attr = dom.has_attribute(node_id, "async").unwrap_or(false);
    return Ok(Value::Bool(force_async || async_attr));
  }
  Ok(Value::Bool(dom.has_attribute(node_id, &attr).unwrap_or(false)))
}

fn element_reflected_bool_set_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };

  let attr = native_slot_string(scope, callee)?;
  let Some((mut dom_ptr, node_id)) = dom_node_id_from_obj(scope, obj)? else {
    return Ok(Value::Undefined);
  };

  let present_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let present = scope.heap().to_boolean(present_value)?;

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  if attr == "async" && is_html_script_element(dom, node_id) {
    // HTMLScriptElement.async setter:
    // - If value is true: ensure the `async` content attribute is present.
    // - If value is false: set the element's "force async" flag to false and remove the `async`
    //   content attribute.
    //
    // Note: adding the `async` content attribute clears the "force async" flag via dom2's attribute
    // mutation hooks, so we only need to explicitly clear it for the `false` path here.
    if !present {
      if let Err(err) = dom.set_script_force_async(node_id, false) {
        return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
      }
    }
    if let Err(err) = dom.set_bool_attribute(node_id, "async", present) {
      return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
    }
    return Ok(Value::Undefined);
  }
  if let Err(err) = dom.set_bool_attribute(node_id, &attr, present) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Value::Undefined)
}

fn is_html_script_element(dom: &dom2::Document, node_id: NodeId) -> bool {
  match &dom.node(node_id).kind {
    dom2::NodeKind::Element {
      tag_name, namespace, ..
    } => {
      tag_name.eq_ignore_ascii_case("script")
        && (namespace.is_empty() || namespace == crate::dom::HTML_NAMESPACE)
    }
    _ => false,
  }
}

fn current_base_url_for_dynamic_scripts(vm: &Vm) -> Option<String> {
  vm
    .user_data::<WindowRealmUserData>()
    .and_then(|data| data.base_url.clone())
}

fn build_dynamic_script_spec(
  dom: &dom2::Document,
  script: NodeId,
  base_url: Option<String>,
) -> crate::js::ScriptElementSpec {
  let async_attr = dom.has_attribute(script, "async").unwrap_or(false);
  let defer_attr = dom.has_attribute(script, "defer").unwrap_or(false);
  let nomodule_attr = dom.has_attribute(script, "nomodule").unwrap_or(false);
  let crossorigin =
    super::parse_crossorigin_attr(dom.get_attribute(script, "crossorigin").ok().flatten());
  let (integrity_attr_present, integrity) =
    super::clamp_integrity_attribute(dom.get_attribute(script, "integrity").ok().flatten());
  let referrer_policy = dom
    .get_attribute(script, "referrerpolicy")
    .ok()
    .flatten()
    .and_then(crate::resource::ReferrerPolicy::from_attribute);

  let base_for_resolve = base_url.as_deref();
  let raw_src = dom.get_attribute(script, "src").ok().flatten();
  let src_attr_present = raw_src.is_some();
  let src = raw_src.and_then(|raw| resolve_script_src_at_parse_time(base_for_resolve, raw));

  let mut inline_text = String::new();
  for &child in &dom.node(script).children {
    if let NodeKind::Text { content } = &dom.node(child).kind {
      inline_text.push_str(content);
    }
  }

  crate::js::ScriptElementSpec {
    base_url,
    src,
    src_attr_present,
    inline_text,
    async_attr,
    defer_attr,
    nomodule_attr,
    crossorigin,
    integrity_attr_present,
    integrity,
    referrer_policy,
    parser_inserted: false,
    force_async: dom.node(script).script_force_async,
    node_id: Some(script),
    script_type: super::determine_script_type_dom2(dom, script),
  }
}

fn node_root_is_shadow_root(dom: &dom2::Document, mut node: NodeId) -> bool {
  loop {
    match &dom.node(node).kind {
      NodeKind::ShadowRoot { .. } => return true,
      NodeKind::Document { .. } => return false,
      _ => {}
    }

    // DOM's "root" concept treats ShadowRoot as the root of a separate tree (i.e. its parent is
    // null). `dom2` currently stores ShadowRoot nodes in the main tree with a parent pointer (the
    // host element) so that the renderer can traverse them. For `currentScript`, we still need the
    // DOM notion of root, so we stop when we see a ShadowRoot.
    let Some(parent) = dom.node(node).parent else {
      return false;
    };
    node = parent;
  }
}

fn queue_dynamic_script_task_inline(
  script: NodeId,
  source_name: String,
  source_text: String,
  nomodule_attr: bool,
) -> Result<(), VmError> {
  let Some(event_loop) = runtime::current_event_loop_mut::<WindowHostState>() else {
    return Ok(());
  };

  event_loop
    .queue_task(TaskSource::Script, move |host, event_loop| {
      // `nomodule` classic scripts must be suppressed when module scripts are supported.
      if host.js_execution_options().supports_module_scripts && nomodule_attr {
        return Ok(());
      }

      host
        .js_execution_options()
        .check_script_source(&source_text, "source=inline")?;

      let new_current_script = {
        let dom = host.dom();
        (dom.is_connected_for_scripting(script) && !node_root_is_shadow_root(dom, script)).then_some(script)
      };

      let current_script_state = host.document_host().current_script_handle().clone();
      let mut orchestrator = ScriptOrchestrator::new();
      orchestrator.execute_with_current_script_state_resolved(
        &current_script_state,
        new_current_script,
        || {
          host.exec_script_with_name_in_event_loop(event_loop, source_name, source_text)?;
          Ok(())
        },
      )
    })
    .map_err(|_| VmError::TypeError("Failed to queue dynamic script task"))?;

  Ok(())
}

fn queue_dynamic_script_task_external(
  script: NodeId,
  url: String,
  destination: FetchDestination,
  nomodule_attr: bool,
) -> Result<(), VmError> {
  let Some(event_loop) = runtime::current_event_loop_mut::<WindowHostState>() else {
    return Ok(());
  };

  event_loop
    .queue_task(TaskSource::Script, move |host, event_loop| {
      if host.js_execution_options().supports_module_scripts && nomodule_attr {
        return Ok(());
      }

      let req = FetchRequest::new(&url, destination).with_referrer_url(&host.document_url);
      let res = host.fetcher().fetch_with_request(req)?;
      ensure_script_mime_sane(&res, &url)?;
      let source_text = crate::js::script_encoding::decode_classic_script_bytes(
        &res.bytes,
        res.content_type.as_deref(),
        encoding_rs::UTF_8,
      );
      host
        .js_execution_options()
        .check_script_source(&source_text, "source=external")?;

      let new_current_script = {
        let dom = host.dom();
        (dom.is_connected_for_scripting(script) && !node_root_is_shadow_root(dom, script)).then_some(script)
      };

      let current_script_state = host.document_host().current_script_handle().clone();
      let mut orchestrator = ScriptOrchestrator::new();
      orchestrator.execute_with_current_script_state_resolved(
        &current_script_state,
        new_current_script,
        || {
          host.exec_script_with_name_in_event_loop(event_loop, url, source_text)?;
          Ok(())
        },
      )
    })
    .map_err(|_| VmError::TypeError("Failed to queue dynamic script task"))?;

  Ok(())
}

fn prepare_dynamic_script(dom: &mut dom2::Document, script: NodeId, base_url: &Option<String>) -> Result<(), VmError> {
  if !is_html_script_element(dom, script) {
    return Ok(());
  }

  // HTML element post-connection steps: parser-inserted scripts are prepared by the parser, not by
  // DOM insertion.
  if dom.node(script).script_parser_document {
    return Ok(());
  }

  // HTML: scripts inside inert `<template>` contents are treated as disconnected and must not
  // execute.
  if !dom.is_connected_for_scripting(script) {
    return Ok(());
  }

  // HTML: do nothing when "already started" is true.
  if dom.node(script).script_already_started {
    return Ok(());
  }

  let spec = build_dynamic_script_spec(dom, script, base_url.clone());

  // HTML: if there is no `src` attribute and the inline text is empty, do nothing. Importantly,
  // this must *not* set the "already started" flag so later `src`/text mutations can trigger
  // preparation.
  if !spec.src_attr_present && spec.inline_text.is_empty() {
    return Ok(());
  }

  dom.node_mut(script).script_already_started = true;

  // Only classic scripts are executed by this vm-js DOM integration helper for now.
  if spec.script_type != ScriptType::Classic {
    return Ok(());
  }

  if spec.src_attr_present {
    if let Some(url) = spec.src {
      let destination = if spec.crossorigin.is_some() {
        FetchDestination::ScriptCors
      } else {
        FetchDestination::Script
      };
      return queue_dynamic_script_task_external(script, url, destination, spec.nomodule_attr);
    }
    return Ok(());
  }

  // Inline script: queue as a task to keep DOM mutation calls non-reentrant.
  let source_name = format!("<script {}>", script.index());
  let source_text = spec.inline_text;
  queue_dynamic_script_task_inline(script, source_name, source_text, spec.nomodule_attr)
}

fn collect_html_script_elements(dom: &dom2::Document, node: NodeId, out: &mut Vec<NodeId>) {
  if is_html_script_element(dom, node) {
    out.push(node);
  }
  for &child in &dom.node(node).children {
    collect_html_script_elements(dom, child, out);
  }
}

fn prepare_dynamic_scripts_on_subtree_insertion(
  dom: &mut dom2::Document,
  inserted_root: NodeId,
  base_url: &Option<String>,
) -> Result<(), VmError> {
  let mut scripts = Vec::new();
  collect_html_script_elements(dom, inserted_root, &mut scripts);
  for script in scripts {
    prepare_dynamic_script(dom, script, base_url)?;
  }
  Ok(())
}

fn css_style_get_property_value_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(style_obj) = this else {
    return Err(VmError::TypeError(
      "CSSStyleDeclaration.getPropertyValue must be called on a style object",
    ));
  };
  let Some((dom_ptr, node_id)) = dom_node_id_from_obj(scope, style_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };

  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let name_value = scope.heap_mut().to_string(name_value)?;
  let name = scope
    .heap()
    .get_string(name_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let value = dom.style_get_property_value(node_id, &name);
  Ok(Value::String(scope.alloc_string(&value)?))
}

fn css_style_set_property_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(style_obj) = this else {
    return Err(VmError::TypeError(
      "CSSStyleDeclaration.setProperty must be called on a style object",
    ));
  };
  let Some((mut dom_ptr, node_id)) = dom_node_id_from_obj(scope, style_obj)? else {
    return Ok(Value::Undefined);
  };

  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let name_value = scope.heap_mut().to_string(name_value)?;
  let name = scope
    .heap()
    .get_string(name_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let value_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let value_value = scope.heap_mut().to_string(value_value)?;
  let value = scope
    .heap()
    .get_string(value_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  if let Err(err) = dom.style_set_property(node_id, &name, &value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Value::Undefined)
}

fn css_style_remove_property_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(style_obj) = this else {
    return Err(VmError::TypeError(
      "CSSStyleDeclaration.removeProperty must be called on a style object",
    ));
  };
  let Some((mut dom_ptr, node_id)) = dom_node_id_from_obj(scope, style_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };

  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let name_value = scope.heap_mut().to_string(name_value)?;
  let name = scope
    .heap()
    .get_string(name_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let prev = dom.style_get_property_value(node_id, &name);
  if let Err(err) = dom.style_set_property(node_id, &name, "") {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Value::String(scope.alloc_string(&prev)?))
}

fn css_style_named_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(style_obj) = this else {
    return Err(VmError::TypeError(
      "CSSStyleDeclaration property getter must be called on a style object",
    ));
  };
  let prop = native_slot_string(scope, callee)?;
  let Some((dom_ptr, node_id)) = dom_node_id_from_obj(scope, style_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let value = dom.style_get_property_value(node_id, &prop);
  Ok(Value::String(scope.alloc_string(&value)?))
}

fn css_style_named_set_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(style_obj) = this else {
    return Err(VmError::TypeError(
      "CSSStyleDeclaration property setter must be called on a style object",
    ));
  };
  let prop = native_slot_string(scope, callee)?;
  let Some((mut dom_ptr, node_id)) = dom_node_id_from_obj(scope, style_obj)? else {
    return Ok(Value::Undefined);
  };

  let value_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let value_value = scope.heap_mut().to_string(value_value)?;
  let value = scope
    .heap()
    .get_string(value_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  if let Err(err) = dom.style_set_property(node_id, &prop, &value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Value::Undefined)
}

fn element_class_list_add_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(class_list_obj) = this else {
    return Err(VmError::TypeError(
      "DOMTokenList.add must be called on a classList object",
    ));
  };

  if args.is_empty() {
    return Ok(Value::Undefined);
  }

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.add requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.add must be called on a classList object",
      ));
    }
  };

  let mut tokens: Vec<String> = Vec::with_capacity(args.len());
  for &arg in args {
    let token_value = scope.heap_mut().to_string(arg)?;
    let token = scope
      .heap()
      .get_string(token_value)
      .map(|s| s.to_utf8_lossy())
      .unwrap_or_default();
    tokens.push(token);
  }
  let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "DOMTokenList.add requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("DOMTokenList.add must be called on a classList object"))?;

  match dom.class_list_add(node_id, &token_refs) {
    Ok(_) => Ok(Value::Undefined),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_class_list_remove_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(class_list_obj) = this else {
    return Err(VmError::TypeError(
      "DOMTokenList.remove must be called on a classList object",
    ));
  };

  if args.is_empty() {
    return Ok(Value::Undefined);
  }

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.remove requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.remove must be called on a classList object",
      ));
    }
  };

  let mut tokens: Vec<String> = Vec::with_capacity(args.len());
  for &arg in args {
    let token_value = scope.heap_mut().to_string(arg)?;
    let token = scope
      .heap()
      .get_string(token_value)
      .map(|s| s.to_utf8_lossy())
      .unwrap_or_default();
    tokens.push(token);
  }
  let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "DOMTokenList.remove requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("DOMTokenList.remove must be called on a classList object"))?;

  match dom.class_list_remove(node_id, &token_refs) {
    Ok(_) => Ok(Value::Undefined),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_class_list_contains_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(class_list_obj) = this else {
    return Err(VmError::TypeError(
      "DOMTokenList.contains must be called on a classList object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.contains requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.contains must be called on a classList object",
      ));
    }
  };

  let token_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let token_value = scope.heap_mut().to_string(token_value)?;
  let token = scope
    .heap()
    .get_string(token_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "DOMTokenList.contains requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("DOMTokenList.contains must be called on a classList object")
  })?;

  match dom.class_list_contains(node_id, &token) {
    Ok(result) => Ok(Value::Bool(result)),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_class_list_toggle_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(class_list_obj) = this else {
    return Err(VmError::TypeError(
      "DOMTokenList.toggle must be called on a classList object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.toggle requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.toggle must be called on a classList object",
      ));
    }
  };

  let token_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let token_value = scope.heap_mut().to_string(token_value)?;
  let token = scope
    .heap()
    .get_string(token_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let force = match args.get(1).copied().unwrap_or(Value::Undefined) {
    Value::Undefined => None,
    other => Some(scope.heap().to_boolean(other)?),
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "DOMTokenList.toggle requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("DOMTokenList.toggle must be called on a classList object"))?;

  match dom.class_list_toggle(node_id, &token, force) {
    Ok(result) => Ok(Value::Bool(result)),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_class_list_replace_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(class_list_obj) = this else {
    return Err(VmError::TypeError(
      "DOMTokenList.replace must be called on a classList object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.replace requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.replace must be called on a classList object",
      ));
    }
  };

  let token_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let token_value = scope.heap_mut().to_string(token_value)?;
  let token = scope
    .heap()
    .get_string(token_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let new_token_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let new_token_value = scope.heap_mut().to_string(new_token_value)?;
  let new_token = scope
    .heap()
    .get_string(new_token_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "DOMTokenList.replace requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("DOMTokenList.replace must be called on a classList object"))?;

  match dom.class_list_replace(node_id, &token, &new_token) {
    Ok(result) => Ok(Value::Bool(result)),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_get_attribute_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.getAttribute must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.getAttribute requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.getAttribute must be called on an element object",
      ));
    }
  };

  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let name_value = scope.heap_mut().to_string(name_value)?;
  let name = scope
    .heap()
    .get_string(name_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.getAttribute requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.getAttribute must be called on an element object"))?;

  match dom.get_attribute(node_id, &name) {
    Ok(Some(value)) => Ok(Value::String(scope.alloc_string(value)?)),
    Ok(None) => Ok(Value::Null),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_set_attribute_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.setAttribute must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.setAttribute requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.setAttribute must be called on an element object",
      ));
    }
  };

  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let name_value = scope.heap_mut().to_string(name_value)?;
  let name = scope
    .heap()
    .get_string(name_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let value_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let value_value = scope.heap_mut().to_string(value_value)?;
  let value = scope
    .heap()
    .get_string(value_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.setAttribute requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.setAttribute must be called on an element object"))?;

  if let Err(err) = dom.set_attribute(node_id, &name, &value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  if name.eq_ignore_ascii_case("src") && is_html_script_element(dom, node_id) {
    let base_url = current_base_url_for_dynamic_scripts(vm);
    prepare_dynamic_script(dom, node_id, &base_url)?;
  }

  Ok(Value::Undefined)
}

fn element_remove_attribute_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.removeAttribute must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.removeAttribute requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.removeAttribute must be called on an element object",
      ));
    }
  };

  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let name_value = scope.heap_mut().to_string(name_value)?;
  let name = scope
    .heap()
    .get_string(name_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.removeAttribute requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("Element.removeAttribute must be called on an element object")
  })?;

  if let Err(err) = dom.remove_attribute(node_id, &name) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  if name.eq_ignore_ascii_case("src") && is_html_script_element(dom, node_id) {
    let base_url = current_base_url_for_dynamic_scripts(vm);
    prepare_dynamic_script(dom, node_id, &base_url)?;
  }

  Ok(Value::Undefined)
}

fn element_inner_html_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.innerHTML must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.innerHTML requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.innerHTML must be called on an element object",
      ));
    }
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.innerHTML requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.innerHTML must be called on an element object"))?;

  match dom.inner_html(node_id) {
    Ok(html) => Ok(Value::String(scope.alloc_string(&html)?)),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_inner_html_set_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.innerHTML must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.innerHTML requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.innerHTML must be called on an element object",
      ));
    }
  };

  let html_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let html_value = scope.heap_mut().to_string(html_value)?;
  let html = scope
    .heap()
    .get_string(html_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.innerHTML requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.innerHTML must be called on an element object"))?;

  if let Err(err) = dom.set_inner_html(node_id, &html) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Value::Undefined)
}

fn element_outer_html_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.outerHTML must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.outerHTML requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.outerHTML must be called on an element object",
      ));
    }
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.outerHTML requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.outerHTML must be called on an element object"))?;

  match dom.outer_html(node_id) {
    Ok(html) => Ok(Value::String(scope.alloc_string(&html)?)),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_outer_html_set_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.outerHTML must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.outerHTML requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.outerHTML must be called on an element object",
      ));
    }
  };

  let html_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let html_value = scope.heap_mut().to_string(html_value)?;
  let html = scope
    .heap()
    .get_string(html_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.outerHTML requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.outerHTML must be called on an element object"))?;

  if let Err(err) = dom.set_outer_html(node_id, &html) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Value::Undefined)
}

fn element_insert_adjacent_html_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentHTML must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentHTML requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentHTML must be called on an element object",
      ));
    }
  };

  let position_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let position_value = scope.heap_mut().to_string(position_value)?;
  let position = scope
    .heap()
    .get_string(position_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let html_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let html_value = scope.heap_mut().to_string(html_value)?;
  let html = scope
    .heap()
    .get_string(html_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentHTML requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("Element.insertAdjacentHTML must be called on an element object")
  })?;

  if let Err(err) = dom.insert_adjacent_html(node_id, &position, &html) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Value::Undefined)
}

fn element_insert_adjacent_element_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentElement must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentElement requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentElement must be called on an element object",
      ));
    }
  };

  let position_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let position_value = scope.heap_mut().to_string(position_value)?;
  let position = scope
    .heap()
    .get_string(position_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let element_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::Object(element_obj) = element_value else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentElement requires an element argument",
    ));
  };

  let child_source_id = match scope
    .heap()
    .object_get_own_data_property_value(element_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentElement requires an element argument",
      ));
    }
  };
  if child_source_id != source_id {
    return Err(VmError::TypeError(
      "Element.insertAdjacentElement cannot move nodes between documents",
    ));
  }

  let child_index = match scope
    .heap()
    .object_get_own_data_property_value(element_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentElement requires an element argument",
      ));
    }
  };

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentElement requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("Element.insertAdjacentElement must be called on an element object")
  })?;
  let child_node_id = dom.node_id_from_index(child_index).map_err(|_| {
    VmError::TypeError("Element.insertAdjacentElement requires an element argument")
  })?;

  match dom.insert_adjacent_element(node_id, &position, child_node_id) {
    Ok(Some(_)) => Ok(element_value),
    Ok(None) => Ok(Value::Null),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_insert_adjacent_text_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentText must be called on an element object",
    ));
  };

  let source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &source_id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentText requires a DOM-backed document",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentText must be called on an element object",
      ));
    }
  };

  let position_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let position_value = scope.heap_mut().to_string(position_value)?;
  let position = scope
    .heap()
    .get_string(position_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let text_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let text_value = scope.heap_mut().to_string(text_value)?;
  let text = scope
    .heap()
    .get_string(text_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(mut dom_ptr) = dom_for_source(source_id) else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentText requires a DOM-backed document",
    ));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_mut() };
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("Element.insertAdjacentText must be called on an element object")
  })?;

  if let Err(err) = dom.insert_adjacent_text(node_id, &position, &text) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Value::Undefined)
}

fn document_current_script_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let id_key = alloc_key(scope, CURRENT_SCRIPT_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Null),
  };

  let Some(node_id) = current_script_for_source(source_id) else {
    return Ok(Value::Null);
  };
  get_or_create_node_wrapper(vm, scope, document_obj, node_id)
}

fn document_ready_state_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::String(scope.alloc_string("complete")?));
  };

  let id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let source_id = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &id_key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => {
      return Ok(Value::String(scope.alloc_string("complete")?));
    }
  };

  let Some(dom_ptr) = dom_for_source(source_id) else {
    return Ok(Value::String(scope.alloc_string("complete")?));
  };
  // SAFETY: DOM sources are registered/unregistered by the Rust host; the pointer is valid for the
  // lifetime of the associated host document.
  let dom = unsafe { dom_ptr.as_ref() };

  Ok(Value::String(scope.alloc_string(dom.ready_state().as_str())?))
}

fn document_base_uri_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let base_url = vm
    .user_data_mut::<WindowRealmUserData>()
    .and_then(|data| data.base_url.clone())
    .unwrap_or_default();
  Ok(Value::String(scope.alloc_string(&base_url)?))
}

fn document_cookie_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let cookie = match vm.user_data_mut::<WindowRealmUserData>() {
    Some(data) => {
      if let Some(fetcher) = data.cookie_fetcher.as_ref() {
        if let Some(header) = fetcher.cookie_header_value(&data.document_url) {
          data.cookie_jar.replace_from_cookie_header(&header);
        }
      }
      data.cookie_jar.cookie_string()
    }
    None => String::new(),
  };
  Ok(Value::String(scope.alloc_string(&cookie)?))
}

fn document_cookie_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
    return Ok(Value::Undefined);
  };

  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let value = match value {
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };

  let s = scope.heap().get_string(value)?;
  if s.as_code_units().len() > MAX_COOKIE_STRING_BYTES {
    return Ok(Value::Undefined);
  }

  let cookie_string = s.to_utf8_lossy();
  if let Some(fetcher) = data.cookie_fetcher.as_ref() {
    fetcher.store_cookie_from_document(&data.document_url, &cookie_string);
  }
  data.cookie_jar.set_cookie_string(&cookie_string);
  Ok(Value::Undefined)
}

fn document_text_content_get_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Document.textContent is always null (DOM Standard).
  Ok(Value::Null)
}

fn document_text_content_set_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // Setting textContent on a Document is a no-op.
  Ok(Value::Undefined)
}

fn throw_document_write_range_error(vm: &mut Vm, scope: &mut Scope<'_>, message: &str) -> VmError {
  if let Some(intr) = vm.intrinsics() {
    match vm_js::new_range_error(scope, intr, message) {
      Ok(value) => VmError::Throw(value),
      Err(err) => err,
    }
  } else {
    VmError::TypeError("RangeError")
  }
}

fn document_write_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  document_write_impl(vm, scope, args, false)
}

fn document_writeln_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  document_write_impl(vm, scope, args, true)
}

fn document_write_impl(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  args: &[Value],
  append_newline: bool,
) -> Result<Value, VmError> {
  let max_bytes_per_call = match current_document_write_state_mut() {
    Some(state) if state.parsing_active() => state.max_bytes_per_call(),
    _ => {
      // Deterministic subset of HTML's ignore-destructive-writes behavior:
      // when no streaming parser is active, treat `document.write()` as a no-op instead of
      // implicitly calling `document.open()` and clearing the document.
      return Ok(Value::Undefined);
    }
  };

  let mut out = String::new();
  for &arg in args {
    let s_handle = match arg {
      Value::String(s) => s,
      other => scope.heap_mut().to_string(other)?,
    };
    let s = scope.heap().get_string(s_handle)?;
    if s.as_code_units().len() > max_bytes_per_call.saturating_sub(out.len()) {
      return Err(throw_document_write_range_error(
        vm,
        scope,
        &format!("document.write exceeded max bytes per call (limit={max_bytes_per_call})"),
      ));
    }
    out.push_str(&s.to_utf8_lossy());
    if out.len() > max_bytes_per_call {
      return Err(throw_document_write_range_error(
        vm,
        scope,
        &format!("document.write exceeded max bytes per call (limit={max_bytes_per_call})"),
      ));
    }
  }

  if append_newline {
    if out.len() >= max_bytes_per_call {
      return Err(throw_document_write_range_error(
        vm,
        scope,
        &format!("document.write exceeded max bytes per call (limit={max_bytes_per_call})"),
      ));
    }
    out.push('\n');
  }

  let Some(state) = current_document_write_state_mut() else {
    return Ok(Value::Undefined);
  };

  match state.try_enqueue(&out) {
    Ok(()) => Ok(Value::Undefined),
    Err(DocumentWriteLimitError::NotParsing) => Ok(Value::Undefined),
    Err(DocumentWriteLimitError::TooManyCalls { limit }) => Err(throw_document_write_range_error(
      vm,
      scope,
      &format!("document.write exceeded max call count (limit={limit})"),
    )),
    Err(DocumentWriteLimitError::PerCallBytesExceeded { len, limit }) => Err(
      throw_document_write_range_error(
        vm,
        scope,
        &format!("document.write exceeded max bytes per call (len={len}, limit={limit})"),
      ),
    ),
    Err(DocumentWriteLimitError::TotalBytesExceeded { current, add, limit }) => Err(
      throw_document_write_range_error(
        vm,
        scope,
        &format!(
          "document.write exceeded max cumulative bytes (current={current}, add={add}, limit={limit})"
        ),
      ),
    ),
  }
}

fn init_window_globals(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
  config: &WindowRealmConfig,
) -> Result<(Option<u64>, Option<u64>, Option<u64>), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();

  // Ensure `DOMException` exists early: many real-world libraries use it for quota errors, token
  // validation, etc.
  DomExceptionClassVmJs::install(vm, &mut scope, realm)?;

  let global_this_key = alloc_key(&mut scope, "globalThis")?;
  let window_key = alloc_key(&mut scope, "window")?;
  let self_key = alloc_key(&mut scope, "self")?;
  let top_key = alloc_key(&mut scope, "top")?;
  let parent_key = alloc_key(&mut scope, "parent")?;
  let console_key = alloc_key(&mut scope, "console")?;
  let location_key = alloc_key(&mut scope, "location")?;
  let document_key = alloc_key(&mut scope, "document")?;
  let session_storage_key = alloc_key(&mut scope, "sessionStorage")?;
  let local_storage_key = alloc_key(&mut scope, "localStorage")?;

  let href_key = alloc_key(&mut scope, "href")?;
  let assign_key = alloc_key(&mut scope, "assign")?;
  let replace_key = alloc_key(&mut scope, "replace")?;
  let protocol_key = alloc_key(&mut scope, "protocol")?;
  let host_key = alloc_key(&mut scope, "host")?;
  let hostname_key = alloc_key(&mut scope, "hostname")?;
  let port_key = alloc_key(&mut scope, "port")?;
  let pathname_key = alloc_key(&mut scope, "pathname")?;
  let search_key = alloc_key(&mut scope, "search")?;
  let hash_key = alloc_key(&mut scope, "hash")?;
  let document_url_key = alloc_key(&mut scope, "URL")?;

  let url_s = scope.alloc_string(&config.document_url)?;
  scope.push_root(Value::String(url_s))?;
  let url_v = Value::String(url_s);

  // HTML's serialized origin is "null" for non-HTTP(S) URLs or opaque origins.
  let origin_str = serialized_origin_for_document_url(&config.document_url);
  let origin_s = scope.alloc_string(&origin_str)?;
  scope.push_root(Value::String(origin_s))?;
  let origin_v = Value::String(origin_s);

  let is_secure_context_v = Value::Bool(is_secure_context_for_document_url(&config.document_url));
  let cross_origin_isolated_v = Value::Bool(false);

  let location_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(location_obj))?;
  // Keep the document URL on the location object so the href getter can access it.
  let location_url_key = alloc_key(&mut scope, LOCATION_URL_KEY)?;
  scope.define_property(location_obj, location_url_key, data_desc(url_v))?;

  let href_get_call_id = vm.register_native_call(location_href_get_native)?;
  let href_get_name = scope.alloc_string("get href")?;
  scope.push_root(Value::String(href_get_name))?;
  let href_get_func = scope.alloc_native_function(href_get_call_id, None, href_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(href_get_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(href_get_func))?;

  let href_set_call_id = vm.register_native_call(location_href_set_native)?;
  let href_set_name = scope.alloc_string("set href")?;
  scope.push_root(Value::String(href_set_name))?;
  let href_set_func = scope.alloc_native_function(href_set_call_id, None, href_set_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(href_set_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(href_set_func))?;

  scope.define_property(
    location_obj,
    href_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(href_get_func),
        set: Value::Object(href_set_func),
      },
    },
  )?;

  let assign_call_id = vm.register_native_call(location_assign_native)?;
  let assign_name = scope.alloc_string("assign")?;
  scope.push_root(Value::String(assign_name))?;
  let assign_func = scope.alloc_native_function(assign_call_id, None, assign_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(assign_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(assign_func))?;
  scope.define_property(location_obj, assign_key, data_desc(Value::Object(assign_func)))?;

  let replace_call_id = vm.register_native_call(location_replace_native)?;
  let replace_name = scope.alloc_string("replace")?;
  scope.push_root(Value::String(replace_name))?;
  let replace_func = scope.alloc_native_function(replace_call_id, None, replace_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(replace_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(replace_func))?;
  scope.define_property(location_obj, replace_key, data_desc(Value::Object(replace_func)))?;

  let location_set_call_id = vm.register_native_call(location_set_unimplemented_native)?;
  let location_set_name = scope.alloc_string("set location")?;
  scope.push_root(Value::String(location_set_name))?;
  let location_set_func =
    scope.alloc_native_function(location_set_call_id, None, location_set_name, 1)?;
  scope.heap_mut().object_set_prototype(
    location_set_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(location_set_func))?;

  let protocol_get_call_id = vm.register_native_call(location_protocol_get_native)?;
  let protocol_get_name = scope.alloc_string("get protocol")?;
  scope.push_root(Value::String(protocol_get_name))?;
  let protocol_get_func =
    scope.alloc_native_function(protocol_get_call_id, None, protocol_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    protocol_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(protocol_get_func))?;
  scope.define_property(
    location_obj,
    protocol_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(protocol_get_func),
        set: Value::Object(location_set_func),
      },
    },
  )?;

  let host_get_call_id = vm.register_native_call(location_host_get_native)?;
  let host_get_name = scope.alloc_string("get host")?;
  scope.push_root(Value::String(host_get_name))?;
  let host_get_func = scope.alloc_native_function(host_get_call_id, None, host_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(host_get_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(host_get_func))?;
  scope.define_property(
    location_obj,
    host_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(host_get_func),
        set: Value::Object(location_set_func),
      },
    },
  )?;

  let hostname_get_call_id = vm.register_native_call(location_hostname_get_native)?;
  let hostname_get_name = scope.alloc_string("get hostname")?;
  scope.push_root(Value::String(hostname_get_name))?;
  let hostname_get_func =
    scope.alloc_native_function(hostname_get_call_id, None, hostname_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    hostname_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(hostname_get_func))?;
  scope.define_property(
    location_obj,
    hostname_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(hostname_get_func),
        set: Value::Object(location_set_func),
      },
    },
  )?;

  let port_get_call_id = vm.register_native_call(location_port_get_native)?;
  let port_get_name = scope.alloc_string("get port")?;
  scope.push_root(Value::String(port_get_name))?;
  let port_get_func = scope.alloc_native_function(port_get_call_id, None, port_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(port_get_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(port_get_func))?;
  scope.define_property(
    location_obj,
    port_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(port_get_func),
        set: Value::Object(location_set_func),
      },
    },
  )?;

  let pathname_get_call_id = vm.register_native_call(location_pathname_get_native)?;
  let pathname_get_name = scope.alloc_string("get pathname")?;
  scope.push_root(Value::String(pathname_get_name))?;
  let pathname_get_func =
    scope.alloc_native_function(pathname_get_call_id, None, pathname_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    pathname_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(pathname_get_func))?;
  scope.define_property(
    location_obj,
    pathname_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(pathname_get_func),
        set: Value::Object(location_set_func),
      },
    },
  )?;

  let search_get_call_id = vm.register_native_call(location_search_get_native)?;
  let search_get_name = scope.alloc_string("get search")?;
  scope.push_root(Value::String(search_get_name))?;
  let search_get_func =
    scope.alloc_native_function(search_get_call_id, None, search_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    search_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(search_get_func))?;
  scope.define_property(
    location_obj,
    search_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(search_get_func),
        set: Value::Object(location_set_func),
      },
    },
  )?;

  let hash_get_call_id = vm.register_native_call(location_hash_get_native)?;
  let hash_get_name = scope.alloc_string("get hash")?;
  scope.push_root(Value::String(hash_get_name))?;
  let hash_get_func = scope.alloc_native_function(hash_get_call_id, None, hash_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(hash_get_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(hash_get_func))?;
  scope.define_property(
    location_obj,
    hash_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(hash_get_func),
        set: Value::Object(location_set_func),
      },
    },
  )?;

  let origin_key = alloc_key(&mut scope, "origin")?;
  scope.define_property(
    location_obj,
    origin_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: origin_v,
        writable: false,
      },
    },
  )?;

  // Expose `window.location`/`document.location` as an accessor so assignments like
  // `location = "/next"` trigger navigation instead of replacing the Location object.
  let window_location_slots = [Value::Object(location_obj)];
  let window_location_get_call_id = vm.register_native_call(window_location_get_native)?;
  let window_location_get_name = scope.alloc_string("get window.location")?;
  scope.push_root(Value::String(window_location_get_name))?;
  let window_location_get_func = scope.alloc_native_function_with_slots(
    window_location_get_call_id,
    None,
    window_location_get_name,
    0,
    &window_location_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    window_location_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(window_location_get_func))?;

  let window_location_set_call_id = vm.register_native_call(window_location_set_native)?;
  let window_location_set_name = scope.alloc_string("set window.location")?;
  scope.push_root(Value::String(window_location_set_name))?;
  let window_location_set_func = scope.alloc_native_function_with_slots(
    window_location_set_call_id,
    None,
    window_location_set_name,
    1,
    &window_location_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    window_location_set_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(window_location_set_func))?;

  let mut dom_platform: Option<DomPlatform> = config
    .dom_source_id
    .map(|id| DomPlatform::new(&mut scope, realm, id))
    .transpose()?;

  let document_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(document_obj))?;
  if let Some(platform) = dom_platform.as_mut() {
    scope.heap_mut().object_set_prototype(
      document_obj,
      Some(platform.prototype_for(DomInterface::Document)),
    )?;
    // `dom2`'s document node is always index 0.
    platform.register_wrapper(
      scope.heap(),
      document_obj,
      NodeId::from_index(0),
      DomInterface::Document,
    );
  }
  scope.define_property(document_obj, document_url_key, data_desc(url_v))?;

  // Document.baseURI (read-only): the document base URL used for resolving relative URLs.
  //
  // This is distinct from `document.URL` and can change when `<base href>` elements are parsed or
  // inserted.
  let base_uri_key = alloc_key(&mut scope, "baseURI")?;
  let base_uri_call_id = vm.register_native_call(document_base_uri_get_native)?;
  let base_uri_name = scope.alloc_string("get baseURI")?;
  scope.push_root(Value::String(base_uri_name))?;
  let base_uri_func = scope.alloc_native_function(base_uri_call_id, None, base_uri_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(base_uri_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(base_uri_func))?;
  scope.define_property(
    document_obj,
    base_uri_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(base_uri_func),
        set: Value::Undefined,
      },
    },
  )?;

  let document_location_key = alloc_key(&mut scope, "location")?;
  scope.define_property(
    document_obj,
    document_location_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(window_location_get_func),
        set: Value::Object(window_location_set_func),
      },
    },
  )?;

  // Backreference used by DOM event dispatch to map `EventTargetId::Window` back into the JS realm.
  let document_window_key = alloc_key(&mut scope, DOCUMENT_WINDOW_KEY)?;
  scope.define_property(
    document_obj,
    document_window_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(global),
        writable: false,
      },
    },
  )?;

  // `Document.referrer`.
  //
  // Many real-world scripts assume this is a string (even if empty) and call string methods on it.
  // Default to the empty string because FastRender does not currently track navigation history.
  let referrer_key = alloc_key(&mut scope, "referrer")?;
  let referrer_s = scope.alloc_string("")?;
  scope.push_root(Value::String(referrer_s))?;
  scope.define_property(
    document_obj,
    referrer_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(referrer_s),
        writable: false,
      },
    },
  )?;

  // Document state shims.
  //
  // These are frequently read by real-world scripts (e.g. to decide whether to run animations).
  let ready_state_key = alloc_key(&mut scope, "readyState")?;
  let ready_state_call_id = vm.register_native_call(document_ready_state_get_native)?;
  let ready_state_name = scope.alloc_string("get readyState")?;
  scope.push_root(Value::String(ready_state_name))?;
  let ready_state_func =
    scope.alloc_native_function(ready_state_call_id, None, ready_state_name, 0)?;
  scope.heap_mut().object_set_prototype(
    ready_state_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(ready_state_func))?;
  scope.define_property(
    document_obj,
    ready_state_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(ready_state_func),
        set: Value::Undefined,
      },
    },
  )?;

  let visibility_state_key = alloc_key(&mut scope, "visibilityState")?;
  let visibility_state_s = scope.alloc_string("visible")?;
  scope.push_root(Value::String(visibility_state_s))?;
  scope.define_property(
    document_obj,
    visibility_state_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(visibility_state_s),
        writable: false,
      },
    },
  )?;

  let hidden_key = alloc_key(&mut scope, "hidden")?;
  scope.define_property(
    document_obj,
    hidden_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Bool(false),
        writable: false,
      },
    },
  )?;

  // document.write / document.writeln
  let write_key = alloc_key(&mut scope, "write")?;
  let write_call_id = vm.register_native_call(document_write_native)?;
  let write_name = scope.alloc_string("write")?;
  scope.push_root(Value::String(write_name))?;
  let write_func = scope.alloc_native_function(write_call_id, None, write_name, 0)?;
  scope.heap_mut().object_set_prototype(
    write_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(write_func))?;
  scope.define_property(document_obj, write_key, data_desc(Value::Object(write_func)))?;

  let writeln_key = alloc_key(&mut scope, "writeln")?;
  let writeln_call_id = vm.register_native_call(document_writeln_native)?;
  let writeln_name = scope.alloc_string("writeln")?;
  scope.push_root(Value::String(writeln_name))?;
  let writeln_func = scope.alloc_native_function(writeln_call_id, None, writeln_name, 0)?;
  scope.heap_mut().object_set_prototype(
    writeln_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(writeln_func))?;
  scope.define_property(
    document_obj,
    writeln_key,
    data_desc(Value::Object(writeln_func)),
  )?;

  // document.currentScript
  let current_script_key = alloc_key(&mut scope, "currentScript")?;
  let current_script_call_id = vm.register_native_call(document_current_script_get_native)?;
  let current_script_name = scope.alloc_string("get currentScript")?;
  scope.push_root(Value::String(current_script_name))?;
  let current_script_func =
    scope.alloc_native_function(current_script_call_id, None, current_script_name, 0)?;
  scope.heap_mut().object_set_prototype(
    current_script_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(current_script_func))?;

  if let Some(dom_source_id) = config.dom_source_id {
    let dom_source_key = alloc_key(&mut scope, DOM_SOURCE_ID_KEY)?;
    scope.define_property(
      document_obj,
      dom_source_key,
      data_desc(Value::Number(dom_source_id as f64)),
    )?;

    // Treat `window.document` as the canonical Node wrapper for `NodeId(0)` so any Node traversal
    // returning the document node preserves object identity and uses Node shims (e.g. `textContent`)
    // consistently.
    let node_id_key = alloc_key(&mut scope, NODE_ID_KEY)?;
    scope.define_property(document_obj, node_id_key, data_desc(Value::Number(0.0)))?;

    let wrapper_document_key = alloc_key(&mut scope, WRAPPER_DOCUMENT_KEY)?;
    scope.define_property(
      document_obj,
      wrapper_document_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Object(document_obj),
          writable: false,
        },
      },
    )?;
  }

  // Document.textContent (from Node): always null.
  let document_text_content_key = alloc_key(&mut scope, "textContent")?;
  let document_text_content_get_call_id = vm.register_native_call(document_text_content_get_native)?;
  let document_text_content_get_name = scope.alloc_string("get textContent")?;
  scope.push_root(Value::String(document_text_content_get_name))?;
  let document_text_content_get_func = scope.alloc_native_function(
    document_text_content_get_call_id,
    None,
    document_text_content_get_name,
    0,
  )?;
  scope.heap_mut().object_set_prototype(
    document_text_content_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(document_text_content_get_func))?;

  let document_text_content_set_call_id = vm.register_native_call(document_text_content_set_native)?;
  let document_text_content_set_name = scope.alloc_string("set textContent")?;
  scope.push_root(Value::String(document_text_content_set_name))?;
  let document_text_content_set_func = scope.alloc_native_function(
    document_text_content_set_call_id,
    None,
    document_text_content_set_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    document_text_content_set_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(document_text_content_set_func))?;

  scope.define_property(
    document_obj,
    document_text_content_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(document_text_content_get_func),
        set: Value::Object(document_text_content_set_func),
      },
    },
  )?;

  // document.documentElement
  let document_element_key = alloc_key(&mut scope, "documentElement")?;
  let document_element_call_id = vm.register_native_call(document_document_element_get_native)?;
  let document_element_name = scope.alloc_string("get documentElement")?;
  scope.push_root(Value::String(document_element_name))?;
  let document_element_func =
    scope.alloc_native_function(document_element_call_id, None, document_element_name, 0)?;
  scope.heap_mut().object_set_prototype(
    document_element_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(document_element_func))?;
  scope.define_property(
    document_obj,
    document_element_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(document_element_func),
        set: Value::Undefined,
      },
    },
  )?;

  // document.head
  let document_head_key = alloc_key(&mut scope, "head")?;
  let document_head_call_id = vm.register_native_call(document_head_get_native)?;
  let document_head_name = scope.alloc_string("get head")?;
  scope.push_root(Value::String(document_head_name))?;
  let document_head_func =
    scope.alloc_native_function(document_head_call_id, None, document_head_name, 0)?;
  scope.heap_mut().object_set_prototype(
    document_head_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(document_head_func))?;
  scope.define_property(
    document_obj,
    document_head_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(document_head_func),
        set: Value::Undefined,
      },
    },
  )?;

  // document.body
  let document_body_key = alloc_key(&mut scope, "body")?;
  let document_body_call_id = vm.register_native_call(document_body_get_native)?;
  let document_body_name = scope.alloc_string("get body")?;
  scope.push_root(Value::String(document_body_name))?;
  let document_body_func =
    scope.alloc_native_function(document_body_call_id, None, document_body_name, 0)?;
  scope.heap_mut().object_set_prototype(
    document_body_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(document_body_func))?;
  scope.define_property(
    document_obj,
    document_body_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(document_body_func),
        set: Value::Undefined,
      },
    },
  )?;

  // document.write / document.writeln
  let write_key = alloc_key(&mut scope, "write")?;
  let write_call_id = vm.register_native_call(document_write_native)?;
  let write_name = scope.alloc_string("write")?;
  scope.push_root(Value::String(write_name))?;
  let write_func = scope.alloc_native_function(write_call_id, None, write_name, 0)?;
  scope.heap_mut().object_set_prototype(
    write_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(write_func))?;
  scope.define_property(document_obj, write_key, data_desc(Value::Object(write_func)))?;

  let writeln_key = alloc_key(&mut scope, "writeln")?;
  let writeln_call_id = vm.register_native_call(document_writeln_native)?;
  let writeln_name = scope.alloc_string("writeln")?;
  scope.push_root(Value::String(writeln_name))?;
  let writeln_func = scope.alloc_native_function(writeln_call_id, None, writeln_name, 0)?;
  scope.heap_mut().object_set_prototype(
    writeln_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(writeln_func))?;
  scope.define_property(
    document_obj,
    writeln_key,
    data_desc(Value::Object(writeln_func)),
  )?;

  // document.getElementById
  let get_element_by_id_key = alloc_key(&mut scope, "getElementById")?;
  let get_element_by_id_call_id = vm.register_native_call(document_get_element_by_id_native)?;
  let get_element_by_id_name = scope.alloc_string("getElementById")?;
  scope.push_root(Value::String(get_element_by_id_name))?;
  let get_element_by_id_func =
    scope.alloc_native_function(get_element_by_id_call_id, None, get_element_by_id_name, 1)?;
  scope.heap_mut().object_set_prototype(
    get_element_by_id_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(get_element_by_id_func))?;
  scope.define_property(
    document_obj,
    get_element_by_id_key,
    data_desc(Value::Object(get_element_by_id_func)),
  )?;

  // document.querySelector
  let query_selector_key = alloc_key(&mut scope, "querySelector")?;
  let query_selector_call_id = vm.register_native_call(document_query_selector_native)?;
  let query_selector_name = scope.alloc_string("querySelector")?;
  scope.push_root(Value::String(query_selector_name))?;
  let query_selector_func =
    scope.alloc_native_function(query_selector_call_id, None, query_selector_name, 1)?;
  scope.heap_mut().object_set_prototype(
    query_selector_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(query_selector_func))?;
  scope.define_property(
    document_obj,
    query_selector_key,
    data_desc(Value::Object(query_selector_func)),
  )?;

  // document.querySelectorAll
  let query_selector_all_key = alloc_key(&mut scope, "querySelectorAll")?;
  let query_selector_all_call_id = vm.register_native_call(document_query_selector_all_native)?;
  let query_selector_all_name = scope.alloc_string("querySelectorAll")?;
  scope.push_root(Value::String(query_selector_all_name))?;
  let query_selector_all_func =
    scope.alloc_native_function(query_selector_all_call_id, None, query_selector_all_name, 1)?;
  scope.heap_mut().object_set_prototype(
    query_selector_all_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(query_selector_all_func))?;
  scope.define_property(
    document_obj,
    query_selector_all_key,
    data_desc(Value::Object(query_selector_all_func)),
  )?;

  // document.createElement
  let create_element_key = alloc_key(&mut scope, "createElement")?;
  let create_element_call_id = vm.register_native_call(document_create_element_native)?;
  let create_element_name = scope.alloc_string("createElement")?;
  scope.push_root(Value::String(create_element_name))?;
  let create_element_func =
    scope.alloc_native_function(create_element_call_id, None, create_element_name, 1)?;
  scope.heap_mut().object_set_prototype(
    create_element_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(create_element_func))?;
  scope.define_property(
    document_obj,
    create_element_key,
    data_desc(Value::Object(create_element_func)),
  )?;

  // document.createTextNode
  let create_text_node_key = alloc_key(&mut scope, "createTextNode")?;
  let create_text_node_call_id = vm.register_native_call(document_create_text_node_native)?;
  let create_text_node_name = scope.alloc_string("createTextNode")?;
  scope.push_root(Value::String(create_text_node_name))?;
  let create_text_node_func =
    scope.alloc_native_function(create_text_node_call_id, None, create_text_node_name, 1)?;
  scope.heap_mut().object_set_prototype(
    create_text_node_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(create_text_node_func))?;
  scope.define_property(
    document_obj,
    create_text_node_key,
    data_desc(Value::Object(create_text_node_func)),
  )?;

  // document.createComment
  let create_comment_key = alloc_key(&mut scope, "createComment")?;
  let create_comment_call_id = vm.register_native_call(document_create_comment_native)?;
  let create_comment_name = scope.alloc_string("createComment")?;
  scope.push_root(Value::String(create_comment_name))?;
  let create_comment_func =
    scope.alloc_native_function(create_comment_call_id, None, create_comment_name, 1)?;
  scope.heap_mut().object_set_prototype(
    create_comment_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(create_comment_func))?;
  scope.define_property(
    document_obj,
    create_comment_key,
    data_desc(Value::Object(create_comment_func)),
  )?;

  // document.createDocumentFragment
  let create_fragment_key = alloc_key(&mut scope, "createDocumentFragment")?;
  let create_fragment_call_id = vm.register_native_call(document_create_document_fragment_native)?;
  let create_fragment_name = scope.alloc_string("createDocumentFragment")?;
  scope.push_root(Value::String(create_fragment_name))?;
  let create_fragment_func =
    scope.alloc_native_function(create_fragment_call_id, None, create_fragment_name, 0)?;
  scope.heap_mut().object_set_prototype(
    create_fragment_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(create_fragment_func))?;
  scope.define_property(
    document_obj,
    create_fragment_key,
    data_desc(Value::Object(create_fragment_func)),
  )?;

  // --- DOM Events (MVP): Event / CustomEvent / document.createEvent -----------------------------
  //
  // Many real-world bundles include the "CustomEvent polyfill" pattern that calls
  // `document.createEvent("CustomEvent")` + `initCustomEvent`. Install these legacy APIs so such
  // scripts can run without immediately aborting.
  let event_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(event_proto))?;

  let init_event_call_id = vm.register_native_call(event_init_event_native)?;
  let init_event_name = scope.alloc_string("initEvent")?;
  scope.push_root(Value::String(init_event_name))?;
  let init_event_func =
    scope.alloc_native_function(init_event_call_id, None, init_event_name, 3)?;
  scope.heap_mut().object_set_prototype(
    init_event_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(init_event_func))?;
  let init_event_key = alloc_key(&mut scope, "initEvent")?;
  scope.define_property(
    event_proto,
    init_event_key,
    data_desc(Value::Object(init_event_func)),
  )?;

  let prevent_default_call_id = vm.register_native_call(event_prototype_prevent_default_native)?;
  let prevent_default_name = scope.alloc_string("preventDefault")?;
  scope.push_root(Value::String(prevent_default_name))?;
  let prevent_default_func =
    scope.alloc_native_function(prevent_default_call_id, None, prevent_default_name, 0)?;
  scope.heap_mut().object_set_prototype(
    prevent_default_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(prevent_default_func))?;
  let prevent_default_key = alloc_key(&mut scope, "preventDefault")?;
  scope.define_property(
    event_proto,
    prevent_default_key,
    data_desc(Value::Object(prevent_default_func)),
  )?;

  let stop_propagation_call_id =
    vm.register_native_call(event_prototype_stop_propagation_native)?;
  let stop_propagation_name = scope.alloc_string("stopPropagation")?;
  scope.push_root(Value::String(stop_propagation_name))?;
  let stop_propagation_func =
    scope.alloc_native_function(stop_propagation_call_id, None, stop_propagation_name, 0)?;
  scope.heap_mut().object_set_prototype(
    stop_propagation_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(stop_propagation_func))?;
  let stop_propagation_key = alloc_key(&mut scope, "stopPropagation")?;
  scope.define_property(
    event_proto,
    stop_propagation_key,
    data_desc(Value::Object(stop_propagation_func)),
  )?;

  let stop_immediate_call_id =
    vm.register_native_call(event_prototype_stop_immediate_propagation_native)?;
  let stop_immediate_name = scope.alloc_string("stopImmediatePropagation")?;
  scope.push_root(Value::String(stop_immediate_name))?;
  let stop_immediate_func =
    scope.alloc_native_function(stop_immediate_call_id, None, stop_immediate_name, 0)?;
  scope.heap_mut().object_set_prototype(
    stop_immediate_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(stop_immediate_func))?;
  let stop_immediate_key = alloc_key(&mut scope, "stopImmediatePropagation")?;
  scope.define_property(
    event_proto,
    stop_immediate_key,
    data_desc(Value::Object(stop_immediate_func)),
  )?;

  let custom_event_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(custom_event_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(custom_event_proto, Some(event_proto))?;

  let init_custom_event_call_id = vm.register_native_call(custom_event_init_custom_event_native)?;
  let init_custom_event_name = scope.alloc_string("initCustomEvent")?;
  scope.push_root(Value::String(init_custom_event_name))?;
  let init_custom_event_func =
    scope.alloc_native_function(init_custom_event_call_id, None, init_custom_event_name, 4)?;
  scope.heap_mut().object_set_prototype(
    init_custom_event_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(init_custom_event_func))?;
  let init_custom_event_key = alloc_key(&mut scope, "initCustomEvent")?;
  scope.define_property(
    custom_event_proto,
    init_custom_event_key,
    data_desc(Value::Object(init_custom_event_func)),
  )?;

  // Constructors on the global object.
  let prototype_key = alloc_key(&mut scope, "prototype")?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;

  let event_ctor_call_id = vm.register_native_call(event_constructor_native)?;
  let event_ctor_construct_id = vm.register_native_construct(event_constructor_construct_native)?;
  let event_ctor_name = scope.alloc_string("Event")?;
  scope.push_root(Value::String(event_ctor_name))?;
  let event_ctor_func = scope.alloc_native_function(
    event_ctor_call_id,
    Some(event_ctor_construct_id),
    event_ctor_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    event_ctor_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(event_ctor_func))?;
  scope.define_property(
    event_ctor_func,
    prototype_key,
    data_desc(Value::Object(event_proto)),
  )?;
  scope.define_property(
    event_proto,
    constructor_key,
    data_desc(Value::Object(event_ctor_func)),
  )?;
  let event_ctor_key = alloc_key(&mut scope, "Event")?;
  scope.define_property(
    global,
    event_ctor_key,
    data_desc(Value::Object(event_ctor_func)),
  )?;

  let custom_event_ctor_call_id = vm.register_native_call(custom_event_constructor_native)?;
  let custom_event_ctor_construct_id =
    vm.register_native_construct(custom_event_constructor_construct_native)?;
  let custom_event_ctor_name = scope.alloc_string("CustomEvent")?;
  scope.push_root(Value::String(custom_event_ctor_name))?;
  let custom_event_ctor_func = scope.alloc_native_function(
    custom_event_ctor_call_id,
    Some(custom_event_ctor_construct_id),
    custom_event_ctor_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    custom_event_ctor_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(custom_event_ctor_func))?;
  scope.define_property(
    custom_event_ctor_func,
    prototype_key,
    data_desc(Value::Object(custom_event_proto)),
  )?;
  scope.define_property(
    custom_event_proto,
    constructor_key,
    data_desc(Value::Object(custom_event_ctor_func)),
  )?;
  let custom_event_ctor_key = alloc_key(&mut scope, "CustomEvent")?;
  scope.define_property(
    global,
    custom_event_ctor_key,
    data_desc(Value::Object(custom_event_ctor_func)),
  )?;

  // Expose the prototypes on document for `document.createEvent`.
  let event_proto_key = alloc_key(&mut scope, EVENT_PROTOTYPE_KEY)?;
  scope.define_property(
    document_obj,
    event_proto_key,
    data_desc(Value::Object(event_proto)),
  )?;
  let custom_event_proto_key = alloc_key(&mut scope, CUSTOM_EVENT_PROTOTYPE_KEY)?;
  scope.define_property(
    document_obj,
    custom_event_proto_key,
    data_desc(Value::Object(custom_event_proto)),
  )?;

  // document.createEvent(interfaceName)
  let create_event_key = alloc_key(&mut scope, "createEvent")?;
  let create_event_call_id = vm.register_native_call(document_create_event_native)?;
  let create_event_name = scope.alloc_string("createEvent")?;
  scope.push_root(Value::String(create_event_name))?;
  let create_event_func =
    scope.alloc_native_function(create_event_call_id, None, create_event_name, 1)?;
  scope.heap_mut().object_set_prototype(
    create_event_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(create_event_func))?;
  scope.define_property(
    document_obj,
    create_event_key,
    data_desc(Value::Object(create_event_func)),
  )?;

  // EventTarget methods.
  //
  // These shims route listener registration and `dispatchEvent()` through the shared DOM event
  // system (`crate::web::events` + `dom2::Document.events()`), so capture/bubble semantics match
  // the rest of the engine.
  let add_event_listener_call_id =
    vm.register_native_call(event_target_add_event_listener_native)?;
  let add_event_listener_name = scope.alloc_string("addEventListener")?;
  scope.push_root(Value::String(add_event_listener_name))?;
  // EventTarget method native slots:
  // - slot 0: default `this` for global functions (see `event_target_resolve_this`)
  // - slot 1: the realm's global object (used to find `document` for non-DOM EventTargets like
  //   `AbortSignal` / `new EventTarget()`).
  let event_target_global_slots = [Value::Object(global), Value::Object(global)];
  let event_target_method_slots = [Value::Undefined, Value::Object(global)];
  let add_event_listener_global_func = scope.alloc_native_function_with_slots(
    add_event_listener_call_id,
    None,
    add_event_listener_name,
    2,
    &event_target_global_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    add_event_listener_global_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(add_event_listener_global_func))?;
  let add_event_listener_func = scope.alloc_native_function_with_slots(
    add_event_listener_call_id,
    None,
    add_event_listener_name,
    2,
    &event_target_method_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    add_event_listener_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(add_event_listener_func))?;

  let remove_event_listener_call_id =
    vm.register_native_call(event_target_remove_event_listener_native)?;
  let remove_event_listener_name = scope.alloc_string("removeEventListener")?;
  scope.push_root(Value::String(remove_event_listener_name))?;
  let remove_event_listener_global_func = scope.alloc_native_function_with_slots(
    remove_event_listener_call_id,
    None,
    remove_event_listener_name,
    2,
    &event_target_global_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    remove_event_listener_global_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(remove_event_listener_global_func))?;
  let remove_event_listener_func = scope.alloc_native_function_with_slots(
    remove_event_listener_call_id,
    None,
    remove_event_listener_name,
    2,
    &event_target_method_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    remove_event_listener_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(remove_event_listener_func))?;

  let dispatch_event_call_id = vm.register_native_call(event_target_dispatch_event_native)?;
  let dispatch_event_name = scope.alloc_string("dispatchEvent")?;
  scope.push_root(Value::String(dispatch_event_name))?;
  let dispatch_event_global_func = scope.alloc_native_function_with_slots(
    dispatch_event_call_id,
    None,
    dispatch_event_name,
    1,
    &event_target_global_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    dispatch_event_global_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(dispatch_event_global_func))?;
  let dispatch_event_func = scope.alloc_native_function_with_slots(
    dispatch_event_call_id,
    None,
    dispatch_event_name,
    1,
    &event_target_method_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    dispatch_event_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(dispatch_event_func))?;

  let add_event_listener_key = alloc_key(&mut scope, "addEventListener")?;
  // Minimal `EventTarget` constructor + prototype.
  let event_target_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(event_target_proto))?;
  scope.define_property(
    event_target_proto,
    add_event_listener_key,
    data_desc(Value::Object(add_event_listener_func)),
  )?;
  scope.define_property(
    global,
    add_event_listener_key,
    data_desc(Value::Object(add_event_listener_global_func)),
  )?;
  scope.define_property(
    document_obj,
    add_event_listener_key,
    data_desc(Value::Object(add_event_listener_func)),
  )?;
  let remove_event_listener_key = alloc_key(&mut scope, "removeEventListener")?;
  scope.define_property(
    event_target_proto,
    remove_event_listener_key,
    data_desc(Value::Object(remove_event_listener_func)),
  )?;
  scope.define_property(
    global,
    remove_event_listener_key,
    data_desc(Value::Object(remove_event_listener_global_func)),
  )?;
  scope.define_property(
    document_obj,
    remove_event_listener_key,
    data_desc(Value::Object(remove_event_listener_func)),
  )?;
  let dispatch_event_key = alloc_key(&mut scope, "dispatchEvent")?;
  scope.define_property(
    event_target_proto,
    dispatch_event_key,
    data_desc(Value::Object(dispatch_event_func)),
  )?;
  scope.define_property(
    global,
    dispatch_event_key,
    data_desc(Value::Object(dispatch_event_global_func)),
  )?;
  scope.define_property(
    document_obj,
    dispatch_event_key,
    data_desc(Value::Object(dispatch_event_func)),
  )?;

  let event_target_ctor_call_id = vm.register_native_call(event_target_constructor_native)?;
  let event_target_ctor_construct_id =
    vm.register_native_construct(event_target_constructor_construct_native)?;
  let event_target_ctor_name = scope.alloc_string("EventTarget")?;
  scope.push_root(Value::String(event_target_ctor_name))?;
  let event_target_ctor_func = scope.alloc_native_function(
    event_target_ctor_call_id,
    Some(event_target_ctor_construct_id),
    event_target_ctor_name,
    0,
  )?;
  scope.heap_mut().object_set_prototype(
    event_target_ctor_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(event_target_ctor_func))?;
  scope.define_property(
    event_target_ctor_func,
    prototype_key,
    data_desc(Value::Object(event_target_proto)),
  )?;
  scope.define_property(
    event_target_proto,
    constructor_key,
    data_desc(Value::Object(event_target_ctor_func)),
  )?;
  let event_target_key = alloc_key(&mut scope, "EventTarget")?;
  scope.define_property(
    global,
    event_target_key,
    data_desc(Value::Object(event_target_ctor_func)),
  )?;

  // --- Core DOM constructors + prototypes (Node/Element/Document/DocumentFragment/Text) ----------
  //
  // These are needed for `instanceof` checks in the curated WPT DOM tests.
  if let Some(platform) = dom_platform.as_ref() {
    let illegal_ctor_call_id = vm.register_native_call(illegal_dom_constructor_native)?;
    let illegal_ctor_construct_id =
      vm.register_native_construct(illegal_dom_constructor_construct_native)?;

    let node_proto = platform.prototype_for(DomInterface::Node);
    let element_proto = platform.prototype_for(DomInterface::Element);
    let document_proto = platform.prototype_for(DomInterface::Document);
    let document_fragment_proto = platform.prototype_for(DomInterface::DocumentFragment);
    let text_proto = platform.prototype_for(DomInterface::Text);

    let make_illegal_ctor = |scope: &mut Scope<'_>, name: &str| -> Result<GcObject, VmError> {
      let name = scope.alloc_string(name)?;
      scope.push_root(Value::String(name))?;
      let func = scope.alloc_native_function(
        illegal_ctor_call_id,
        Some(illegal_ctor_construct_id),
        name,
        0,
      )?;
      scope.heap_mut().object_set_prototype(
        func,
        Some(realm.intrinsics().function_prototype()),
      )?;
      Ok(func)
    };

    // Node constructor + constants.
    let node_ctor = make_illegal_ctor(&mut scope, "Node")?;
    scope.push_root(Value::Object(node_ctor))?;
    scope.define_property(node_ctor, prototype_key, data_desc(Value::Object(node_proto)))?;
    scope.define_property(
      node_proto,
      constructor_key,
      data_desc(Value::Object(node_ctor)),
    )?;
    let node_key = alloc_key(&mut scope, "Node")?;
    scope.define_property(global, node_key, data_desc(Value::Object(node_ctor)))?;

    for (name, value) in [
      ("ELEMENT_NODE", 1.0),
      ("TEXT_NODE", 3.0),
      ("DOCUMENT_NODE", 9.0),
      ("DOCUMENT_FRAGMENT_NODE", 11.0),
    ] {
      let key = alloc_key(&mut scope, name)?;
      scope.define_property(
        node_ctor,
        key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Number(value),
            writable: false,
          },
        },
      )?;
    }

    let element_ctor = make_illegal_ctor(&mut scope, "Element")?;
    scope.push_root(Value::Object(element_ctor))?;
    scope.define_property(
      element_ctor,
      prototype_key,
      data_desc(Value::Object(element_proto)),
    )?;
    scope.define_property(
      element_proto,
      constructor_key,
      data_desc(Value::Object(element_ctor)),
    )?;
    let element_key = alloc_key(&mut scope, "Element")?;
    scope.define_property(
      global,
      element_key,
      data_desc(Value::Object(element_ctor)),
    )?;

    let document_ctor = make_illegal_ctor(&mut scope, "Document")?;
    scope.push_root(Value::Object(document_ctor))?;
    scope.define_property(
      document_ctor,
      prototype_key,
      data_desc(Value::Object(document_proto)),
    )?;
    scope.define_property(
      document_proto,
      constructor_key,
      data_desc(Value::Object(document_ctor)),
    )?;
    let document_key = alloc_key(&mut scope, "Document")?;
    scope.define_property(
      global,
      document_key,
      data_desc(Value::Object(document_ctor)),
    )?;

    let document_fragment_ctor = make_illegal_ctor(&mut scope, "DocumentFragment")?;
    scope.push_root(Value::Object(document_fragment_ctor))?;
    scope.define_property(
      document_fragment_ctor,
      prototype_key,
      data_desc(Value::Object(document_fragment_proto)),
    )?;
    scope.define_property(
      document_fragment_proto,
      constructor_key,
      data_desc(Value::Object(document_fragment_ctor)),
    )?;
    let document_fragment_key = alloc_key(&mut scope, "DocumentFragment")?;
    scope.define_property(
      global,
      document_fragment_key,
      data_desc(Value::Object(document_fragment_ctor)),
    )?;

    let text_ctor = make_illegal_ctor(&mut scope, "Text")?;
    scope.push_root(Value::Object(text_ctor))?;
    scope.define_property(text_ctor, prototype_key, data_desc(Value::Object(text_proto)))?;
    scope.define_property(
      text_proto,
      constructor_key,
      data_desc(Value::Object(text_ctor)),
    )?;
    let text_key = alloc_key(&mut scope, "Text")?;
    scope.define_property(global, text_key, data_desc(Value::Object(text_ctor)))?;

    // Node prototype accessors and methods used by WPT DOM tests.
    let node_type_get_call_id = vm.register_native_call(node_node_type_get_native)?;
    let node_type_get_name = scope.alloc_string("get nodeType")?;
    scope.push_root(Value::String(node_type_get_name))?;
    let node_type_get_func =
      scope.alloc_native_function(node_type_get_call_id, None, node_type_get_name, 0)?;
    scope.heap_mut().object_set_prototype(
      node_type_get_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(node_type_get_func))?;
    let node_type_key = alloc_key(&mut scope, "nodeType")?;
    scope.define_property(
      node_proto,
      node_type_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(node_type_get_func),
          set: Value::Undefined,
        },
      },
    )?;

    let node_name_get_call_id = vm.register_native_call(node_node_name_get_native)?;
    let node_name_get_name = scope.alloc_string("get nodeName")?;
    scope.push_root(Value::String(node_name_get_name))?;
    let node_name_get_func =
      scope.alloc_native_function(node_name_get_call_id, None, node_name_get_name, 0)?;
    scope.heap_mut().object_set_prototype(
      node_name_get_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(node_name_get_func))?;
    let node_name_key = alloc_key(&mut scope, "nodeName")?;
    scope.define_property(
      node_proto,
      node_name_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(node_name_get_func),
          set: Value::Undefined,
        },
      },
    )?;

    let owner_document_get_call_id = vm.register_native_call(node_owner_document_get_native)?;
    let owner_document_get_name = scope.alloc_string("get ownerDocument")?;
    scope.push_root(Value::String(owner_document_get_name))?;
    let owner_document_get_func =
      scope.alloc_native_function(owner_document_get_call_id, None, owner_document_get_name, 0)?;
    scope.heap_mut().object_set_prototype(
      owner_document_get_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(owner_document_get_func))?;
    let owner_document_key = alloc_key(&mut scope, "ownerDocument")?;
    scope.define_property(
      node_proto,
      owner_document_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(owner_document_get_func),
          set: Value::Undefined,
        },
      },
    )?;

    let is_connected_get_call_id = vm.register_native_call(node_is_connected_get_native)?;
    let is_connected_get_name = scope.alloc_string("get isConnected")?;
    scope.push_root(Value::String(is_connected_get_name))?;
    let is_connected_get_func =
      scope.alloc_native_function(is_connected_get_call_id, None, is_connected_get_name, 0)?;
    scope.heap_mut().object_set_prototype(
      is_connected_get_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(is_connected_get_func))?;
    let is_connected_key = alloc_key(&mut scope, "isConnected")?;
    scope.define_property(
      node_proto,
      is_connected_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(is_connected_get_func),
          set: Value::Undefined,
        },
      },
    )?;

    let child_nodes_get_call_id = vm.register_native_call(node_child_nodes_get_native)?;
    let child_nodes_get_name = scope.alloc_string("get childNodes")?;
    scope.push_root(Value::String(child_nodes_get_name))?;
    let child_nodes_get_func =
      scope.alloc_native_function(child_nodes_get_call_id, None, child_nodes_get_name, 0)?;
    scope.heap_mut().object_set_prototype(
      child_nodes_get_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(child_nodes_get_func))?;
    let child_nodes_key = alloc_key(&mut scope, "childNodes")?;
    scope.define_property(
      node_proto,
      child_nodes_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(child_nodes_get_func),
          set: Value::Undefined,
        },
      },
    )?;

    let parent_element_get_call_id = vm.register_native_call(node_parent_element_get_native)?;
    let parent_element_get_name = scope.alloc_string("get parentElement")?;
    scope.push_root(Value::String(parent_element_get_name))?;
    let parent_element_get_func = scope.alloc_native_function(
      parent_element_get_call_id,
      None,
      parent_element_get_name,
      0,
    )?;
    scope.heap_mut().object_set_prototype(
      parent_element_get_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(parent_element_get_func))?;
    let parent_element_key = alloc_key(&mut scope, "parentElement")?;
    scope.define_property(
      node_proto,
      parent_element_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(parent_element_get_func),
          set: Value::Undefined,
        },
      },
    )?;

    let last_child_get_call_id = vm.register_native_call(node_last_child_get_native)?;
    let last_child_get_name = scope.alloc_string("get lastChild")?;
    scope.push_root(Value::String(last_child_get_name))?;
    let last_child_get_func =
      scope.alloc_native_function(last_child_get_call_id, None, last_child_get_name, 0)?;
    scope.heap_mut().object_set_prototype(
      last_child_get_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(last_child_get_func))?;
    let last_child_key = alloc_key(&mut scope, "lastChild")?;
    scope.define_property(
      node_proto,
      last_child_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(last_child_get_func),
          set: Value::Undefined,
        },
      },
    )?;

    let contains_call_id = vm.register_native_call(node_contains_native)?;
    let contains_name = scope.alloc_string("contains")?;
    scope.push_root(Value::String(contains_name))?;
    let contains_func = scope.alloc_native_function(contains_call_id, None, contains_name, 1)?;
    scope.heap_mut().object_set_prototype(
      contains_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(contains_func))?;
    let contains_key = alloc_key(&mut scope, "contains")?;
    scope.define_property(node_proto, contains_key, data_desc(Value::Object(contains_func)))?;

    let has_child_nodes_call_id = vm.register_native_call(node_has_child_nodes_native)?;
    let has_child_nodes_name = scope.alloc_string("hasChildNodes")?;
    scope.push_root(Value::String(has_child_nodes_name))?;
    let has_child_nodes_func = scope.alloc_native_function(
      has_child_nodes_call_id,
      None,
      has_child_nodes_name,
      0,
    )?;
    scope.heap_mut().object_set_prototype(
      has_child_nodes_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(has_child_nodes_func))?;
    let has_child_nodes_key = alloc_key(&mut scope, "hasChildNodes")?;
    scope.define_property(
      node_proto,
      has_child_nodes_key,
      data_desc(Value::Object(has_child_nodes_func)),
    )?;

    // Element.tagName
    let tag_name_get_call_id = vm.register_native_call(element_tag_name_get_native)?;
    let tag_name_get_name = scope.alloc_string("get tagName")?;
    scope.push_root(Value::String(tag_name_get_name))?;
    let tag_name_get_func =
      scope.alloc_native_function(tag_name_get_call_id, None, tag_name_get_name, 0)?;
    scope.heap_mut().object_set_prototype(
      tag_name_get_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(tag_name_get_func))?;
    let tag_name_key = alloc_key(&mut scope, "tagName")?;
    scope.define_property(
      element_proto,
      tag_name_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(tag_name_get_func),
          set: Value::Undefined,
        },
      },
    )?;

    // DocumentFragment.getElementById
    let frag_get_element_by_id_call_id =
      vm.register_native_call(document_fragment_get_element_by_id_native)?;
    let frag_get_element_by_id_name = scope.alloc_string("getElementById")?;
    scope.push_root(Value::String(frag_get_element_by_id_name))?;
    let frag_get_element_by_id_func = scope.alloc_native_function(
      frag_get_element_by_id_call_id,
      None,
      frag_get_element_by_id_name,
      1,
    )?;
    scope.heap_mut().object_set_prototype(
      frag_get_element_by_id_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(frag_get_element_by_id_func))?;
    let frag_get_element_by_id_key = alloc_key(&mut scope, "getElementById")?;
    scope.define_property(
      document_fragment_proto,
      frag_get_element_by_id_key,
      data_desc(Value::Object(frag_get_element_by_id_func)),
    )?;

    // Text.data
    let text_data_get_call_id = vm.register_native_call(text_data_get_native)?;
    let text_data_get_name = scope.alloc_string("get data")?;
    scope.push_root(Value::String(text_data_get_name))?;
    let text_data_get_func =
      scope.alloc_native_function(text_data_get_call_id, None, text_data_get_name, 0)?;
    scope.heap_mut().object_set_prototype(
      text_data_get_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(text_data_get_func))?;

    let text_data_set_call_id = vm.register_native_call(text_data_set_native)?;
    let text_data_set_name = scope.alloc_string("set data")?;
    scope.push_root(Value::String(text_data_set_name))?;
    let text_data_set_func =
      scope.alloc_native_function(text_data_set_call_id, None, text_data_set_name, 1)?;
    scope.heap_mut().object_set_prototype(
      text_data_set_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
    scope.push_root(Value::Object(text_data_set_func))?;

    let text_data_key = alloc_key(&mut scope, "data")?;
    scope.define_property(
      text_proto,
      text_data_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(text_data_get_func),
          set: Value::Object(text_data_set_func),
        },
      },
    )?;
  }

  // Store shared function objects on document so wrappers can reuse them.
  let add_event_listener_internal_key = alloc_key(&mut scope, EVENT_TARGET_ADD_EVENT_LISTENER_KEY)?;
  scope.define_property(
    document_obj,
    add_event_listener_internal_key,
    data_desc(Value::Object(add_event_listener_func)),
  )?;
  let remove_event_listener_internal_key =
    alloc_key(&mut scope, EVENT_TARGET_REMOVE_EVENT_LISTENER_KEY)?;
  scope.define_property(
    document_obj,
    remove_event_listener_internal_key,
    data_desc(Value::Object(remove_event_listener_func)),
  )?;
  let dispatch_event_internal_key = alloc_key(&mut scope, EVENT_TARGET_DISPATCH_EVENT_KEY)?;
  scope.define_property(
    document_obj,
    dispatch_event_internal_key,
    data_desc(Value::Object(dispatch_event_func)),
  )?;

  // Store shared Node.appendChild function on `document` so wrappers can reuse it.
  let append_child_call_id = vm.register_native_call(node_append_child_native)?;
  let append_child_name = scope.alloc_string("appendChild")?;
  scope.push_root(Value::String(append_child_name))?;
  let append_child_func =
    scope.alloc_native_function(append_child_call_id, None, append_child_name, 1)?;
  scope.heap_mut().object_set_prototype(
    append_child_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(append_child_func))?;
  let append_child_key = alloc_key(&mut scope, NODE_APPEND_CHILD_KEY)?;
  scope.define_property(
    document_obj,
    append_child_key,
    data_desc(Value::Object(append_child_func)),
  )?;

  // Store shared Node.insertBefore function on `document` so wrappers can reuse it.
  let insert_before_call_id = vm.register_native_call(node_insert_before_native)?;
  let insert_before_name = scope.alloc_string("insertBefore")?;
  scope.push_root(Value::String(insert_before_name))?;
  let insert_before_func =
    scope.alloc_native_function(insert_before_call_id, None, insert_before_name, 2)?;
  scope.heap_mut().object_set_prototype(
    insert_before_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(insert_before_func))?;
  let insert_before_key = alloc_key(&mut scope, NODE_INSERT_BEFORE_KEY)?;
  scope.define_property(
    document_obj,
    insert_before_key,
    data_desc(Value::Object(insert_before_func)),
  )?;

  // Store shared Node.removeChild function on `document` so wrappers can reuse it.
  let remove_child_call_id = vm.register_native_call(node_remove_child_native)?;
  let remove_child_name = scope.alloc_string("removeChild")?;
  scope.push_root(Value::String(remove_child_name))?;
  let remove_child_func =
    scope.alloc_native_function(remove_child_call_id, None, remove_child_name, 1)?;
  scope.heap_mut().object_set_prototype(
    remove_child_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(remove_child_func))?;
  let remove_child_key = alloc_key(&mut scope, NODE_REMOVE_CHILD_KEY)?;
  scope.define_property(
    document_obj,
    remove_child_key,
    data_desc(Value::Object(remove_child_func)),
  )?;

  // Store shared Node.replaceChild function on `document` so wrappers can reuse it.
  let replace_child_call_id = vm.register_native_call(node_replace_child_native)?;
  let replace_child_name = scope.alloc_string("replaceChild")?;
  scope.push_root(Value::String(replace_child_name))?;
  let replace_child_func =
    scope.alloc_native_function(replace_child_call_id, None, replace_child_name, 2)?;
  scope.heap_mut().object_set_prototype(
    replace_child_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(replace_child_func))?;
  let replace_child_key = alloc_key(&mut scope, NODE_REPLACE_CHILD_KEY)?;
  scope.define_property(
    document_obj,
    replace_child_key,
    data_desc(Value::Object(replace_child_func)),
  )?;

  // Store shared Node.cloneNode function on `document` so wrappers can reuse it.
  let clone_node_call_id = vm.register_native_call(node_clone_node_native)?;
  let clone_node_name = scope.alloc_string("cloneNode")?;
  scope.push_root(Value::String(clone_node_name))?;
  let clone_node_func =
    scope.alloc_native_function(clone_node_call_id, None, clone_node_name, 1)?;
  scope.heap_mut().object_set_prototype(
    clone_node_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(clone_node_func))?;
  let clone_node_key = alloc_key(&mut scope, NODE_CLONE_NODE_KEY)?;
  scope.define_property(
    document_obj,
    clone_node_key,
    data_desc(Value::Object(clone_node_func)),
  )?;

  // Store shared Node traversal accessors and convenience helpers on `document` so wrappers can
  // reuse them.
  let parent_node_get_call_id = vm.register_native_call(node_parent_node_get_native)?;
  let parent_node_get_name = scope.alloc_string("get parentNode")?;
  scope.push_root(Value::String(parent_node_get_name))?;
  let parent_node_get_func =
    scope.alloc_native_function(parent_node_get_call_id, None, parent_node_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    parent_node_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(parent_node_get_func))?;
  let parent_node_get_key = alloc_key(&mut scope, NODE_PARENT_NODE_GET_KEY)?;
  scope.define_property(
    document_obj,
    parent_node_get_key,
    data_desc(Value::Object(parent_node_get_func)),
  )?;

  let first_child_get_call_id = vm.register_native_call(node_first_child_get_native)?;
  let first_child_get_name = scope.alloc_string("get firstChild")?;
  scope.push_root(Value::String(first_child_get_name))?;
  let first_child_get_func =
    scope.alloc_native_function(first_child_get_call_id, None, first_child_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    first_child_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(first_child_get_func))?;
  let first_child_get_key = alloc_key(&mut scope, NODE_FIRST_CHILD_GET_KEY)?;
  scope.define_property(
    document_obj,
    first_child_get_key,
    data_desc(Value::Object(first_child_get_func)),
  )?;

  let previous_sibling_get_call_id = vm.register_native_call(node_previous_sibling_get_native)?;
  let previous_sibling_get_name = scope.alloc_string("get previousSibling")?;
  scope.push_root(Value::String(previous_sibling_get_name))?;
  let previous_sibling_get_func = scope.alloc_native_function(
    previous_sibling_get_call_id,
    None,
    previous_sibling_get_name,
    0,
  )?;
  scope.heap_mut().object_set_prototype(
    previous_sibling_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(previous_sibling_get_func))?;
  let previous_sibling_get_key = alloc_key(&mut scope, NODE_PREVIOUS_SIBLING_GET_KEY)?;
  scope.define_property(
    document_obj,
    previous_sibling_get_key,
    data_desc(Value::Object(previous_sibling_get_func)),
  )?;

  let next_sibling_get_call_id = vm.register_native_call(node_next_sibling_get_native)?;
  let next_sibling_get_name = scope.alloc_string("get nextSibling")?;
  scope.push_root(Value::String(next_sibling_get_name))?;
  let next_sibling_get_func =
    scope.alloc_native_function(next_sibling_get_call_id, None, next_sibling_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    next_sibling_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(next_sibling_get_func))?;
  let next_sibling_get_key = alloc_key(&mut scope, NODE_NEXT_SIBLING_GET_KEY)?;
  scope.define_property(
    document_obj,
    next_sibling_get_key,
    data_desc(Value::Object(next_sibling_get_func)),
  )?;

  // Store shared Node.textContent getter/setter on `document` so wrappers can reuse them.
  let text_content_get_call_id = vm.register_native_call(node_text_content_get_native)?;
  let text_content_get_name = scope.alloc_string("get textContent")?;
  scope.push_root(Value::String(text_content_get_name))?;
  let text_content_get_func = scope.alloc_native_function(
    text_content_get_call_id,
    None,
    text_content_get_name,
    0,
  )?;
  scope.heap_mut().object_set_prototype(
    text_content_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(text_content_get_func))?;
  let text_content_get_key = alloc_key(&mut scope, NODE_TEXT_CONTENT_GET_KEY)?;
  scope.define_property(
    document_obj,
    text_content_get_key,
    data_desc(Value::Object(text_content_get_func)),
  )?;

  let text_content_set_call_id = vm.register_native_call(node_text_content_set_native)?;
  let text_content_set_name = scope.alloc_string("set textContent")?;
  scope.push_root(Value::String(text_content_set_name))?;
  let text_content_set_func = scope.alloc_native_function(
    text_content_set_call_id,
    None,
    text_content_set_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    text_content_set_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(text_content_set_func))?;
  let text_content_set_key = alloc_key(&mut scope, NODE_TEXT_CONTENT_SET_KEY)?;
  scope.define_property(
    document_obj,
    text_content_set_key,
    data_desc(Value::Object(text_content_set_func)),
  )?;

  if config.dom_source_id.is_some() {
    // Ensure `window.document` exposes Node.textContent semantics when it acts as the canonical
    // wrapper for `NodeId(0)`.
    let text_content_key = alloc_key(&mut scope, "textContent")?;
    scope.define_property(
      document_obj,
      text_content_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(text_content_get_func),
          set: Value::Object(text_content_set_func),
        },
      },
    )?;
  }

  let node_remove_call_id = vm.register_native_call(node_remove_native)?;
  let node_remove_name = scope.alloc_string("remove")?;
  scope.push_root(Value::String(node_remove_name))?;
  let node_remove_func =
    scope.alloc_native_function(node_remove_call_id, None, node_remove_name, 0)?;
  scope.heap_mut().object_set_prototype(
    node_remove_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(node_remove_func))?;
  let node_remove_key = alloc_key(&mut scope, NODE_REMOVE_KEY)?;
  scope.define_property(
    document_obj,
    node_remove_key,
    data_desc(Value::Object(node_remove_func)),
  )?;

  // Store shared Element selector traversal APIs on `document` so wrappers can reuse them.
  let element_query_selector_call_id = vm.register_native_call(element_query_selector_native)?;
  let element_query_selector_name = scope.alloc_string("querySelector")?;
  scope.push_root(Value::String(element_query_selector_name))?;
  let element_query_selector_func = scope.alloc_native_function(
    element_query_selector_call_id,
    None,
    element_query_selector_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    element_query_selector_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(element_query_selector_func))?;
  let element_query_selector_key = alloc_key(&mut scope, ELEMENT_QUERY_SELECTOR_KEY)?;
  scope.define_property(
    document_obj,
    element_query_selector_key,
    data_desc(Value::Object(element_query_selector_func)),
  )?;

  let element_query_selector_all_call_id =
    vm.register_native_call(element_query_selector_all_native)?;
  let element_query_selector_all_name = scope.alloc_string("querySelectorAll")?;
  scope.push_root(Value::String(element_query_selector_all_name))?;
  let element_query_selector_all_func = scope.alloc_native_function(
    element_query_selector_all_call_id,
    None,
    element_query_selector_all_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    element_query_selector_all_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(element_query_selector_all_func))?;
  let element_query_selector_all_key = alloc_key(&mut scope, ELEMENT_QUERY_SELECTOR_ALL_KEY)?;
  scope.define_property(
    document_obj,
    element_query_selector_all_key,
    data_desc(Value::Object(element_query_selector_all_func)),
  )?;

  let element_matches_call_id = vm.register_native_call(element_matches_native)?;
  let element_matches_name = scope.alloc_string("matches")?;
  scope.push_root(Value::String(element_matches_name))?;
  let element_matches_func =
    scope.alloc_native_function(element_matches_call_id, None, element_matches_name, 1)?;
  scope.heap_mut().object_set_prototype(
    element_matches_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(element_matches_func))?;
  let element_matches_key = alloc_key(&mut scope, ELEMENT_MATCHES_KEY)?;
  scope.define_property(
    document_obj,
    element_matches_key,
    data_desc(Value::Object(element_matches_func)),
  )?;

  let element_closest_call_id = vm.register_native_call(element_closest_native)?;
  let element_closest_name = scope.alloc_string("closest")?;
  scope.push_root(Value::String(element_closest_name))?;
  let element_closest_func =
    scope.alloc_native_function(element_closest_call_id, None, element_closest_name, 1)?;
  scope.heap_mut().object_set_prototype(
    element_closest_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(element_closest_func))?;
  let element_closest_key = alloc_key(&mut scope, ELEMENT_CLOSEST_KEY)?;
  scope.define_property(
    document_obj,
    element_closest_key,
    data_desc(Value::Object(element_closest_func)),
  )?;

  // Store shared Element.getAttribute/setAttribute functions on `document` so wrappers can reuse them.
  let get_attribute_call_id = vm.register_native_call(element_get_attribute_native)?;
  let get_attribute_name = scope.alloc_string("getAttribute")?;
  scope.push_root(Value::String(get_attribute_name))?;
  let get_attribute_func =
    scope.alloc_native_function(get_attribute_call_id, None, get_attribute_name, 1)?;
  scope.heap_mut().object_set_prototype(
    get_attribute_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(get_attribute_func))?;
  let get_attribute_key = alloc_key(&mut scope, ELEMENT_GET_ATTRIBUTE_KEY)?;
  scope.define_property(
    document_obj,
    get_attribute_key,
    data_desc(Value::Object(get_attribute_func)),
  )?;

  let set_attribute_call_id = vm.register_native_call(element_set_attribute_native)?;
  let set_attribute_name = scope.alloc_string("setAttribute")?;
  scope.push_root(Value::String(set_attribute_name))?;
  let set_attribute_func =
    scope.alloc_native_function(set_attribute_call_id, None, set_attribute_name, 2)?;
  scope.heap_mut().object_set_prototype(
    set_attribute_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(set_attribute_func))?;
  let set_attribute_key = alloc_key(&mut scope, ELEMENT_SET_ATTRIBUTE_KEY)?;
  scope.define_property(
    document_obj,
    set_attribute_key,
    data_desc(Value::Object(set_attribute_func)),
  )?;

  let remove_attribute_call_id = vm.register_native_call(element_remove_attribute_native)?;
  let remove_attribute_name = scope.alloc_string("removeAttribute")?;
  scope.push_root(Value::String(remove_attribute_name))?;
  let remove_attribute_func = scope.alloc_native_function(
    remove_attribute_call_id,
    None,
    remove_attribute_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    remove_attribute_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(remove_attribute_func))?;
  let remove_attribute_key = alloc_key(&mut scope, ELEMENT_REMOVE_ATTRIBUTE_KEY)?;
  scope.define_property(
    document_obj,
    remove_attribute_key,
    data_desc(Value::Object(remove_attribute_func)),
  )?;

  // Store shared Element.className getter/setter functions on `document` so wrappers can reuse them.
  let class_name_get_call_id = vm.register_native_call(element_class_name_get_native)?;
  let class_name_get_name = scope.alloc_string("get className")?;
  scope.push_root(Value::String(class_name_get_name))?;
  let class_name_get_func =
    scope.alloc_native_function(class_name_get_call_id, None, class_name_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    class_name_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(class_name_get_func))?;
  let class_name_get_key = alloc_key(&mut scope, ELEMENT_CLASS_NAME_GET_KEY)?;
  scope.define_property(
    document_obj,
    class_name_get_key,
    data_desc(Value::Object(class_name_get_func)),
  )?;

  let class_name_set_call_id = vm.register_native_call(element_class_name_set_native)?;
  let class_name_set_name = scope.alloc_string("set className")?;
  scope.push_root(Value::String(class_name_set_name))?;
  let class_name_set_func =
    scope.alloc_native_function(class_name_set_call_id, None, class_name_set_name, 1)?;
  scope.heap_mut().object_set_prototype(
    class_name_set_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(class_name_set_func))?;
  let class_name_set_key = alloc_key(&mut scope, ELEMENT_CLASS_NAME_SET_KEY)?;
  scope.define_property(
    document_obj,
    class_name_set_key,
    data_desc(Value::Object(class_name_set_func)),
  )?;

  // Store shared Element.classList methods on `document` so wrappers can reuse them.
  let class_list_add_call_id = vm.register_native_call(element_class_list_add_native)?;
  let class_list_add_name = scope.alloc_string("add")?;
  scope.push_root(Value::String(class_list_add_name))?;
  let class_list_add_func =
    scope.alloc_native_function(class_list_add_call_id, None, class_list_add_name, 0)?;
  scope.heap_mut().object_set_prototype(
    class_list_add_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(class_list_add_func))?;
  let class_list_add_key = alloc_key(&mut scope, ELEMENT_CLASS_LIST_ADD_KEY)?;
  scope.define_property(
    document_obj,
    class_list_add_key,
    data_desc(Value::Object(class_list_add_func)),
  )?;

  let class_list_remove_call_id = vm.register_native_call(element_class_list_remove_native)?;
  let class_list_remove_name = scope.alloc_string("remove")?;
  scope.push_root(Value::String(class_list_remove_name))?;
  let class_list_remove_func =
    scope.alloc_native_function(class_list_remove_call_id, None, class_list_remove_name, 0)?;
  scope.heap_mut().object_set_prototype(
    class_list_remove_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(class_list_remove_func))?;
  let class_list_remove_key = alloc_key(&mut scope, ELEMENT_CLASS_LIST_REMOVE_KEY)?;
  scope.define_property(
    document_obj,
    class_list_remove_key,
    data_desc(Value::Object(class_list_remove_func)),
  )?;

  let class_list_toggle_call_id = vm.register_native_call(element_class_list_toggle_native)?;
  let class_list_toggle_name = scope.alloc_string("toggle")?;
  scope.push_root(Value::String(class_list_toggle_name))?;
  let class_list_toggle_func =
    scope.alloc_native_function(class_list_toggle_call_id, None, class_list_toggle_name, 1)?;
  scope.heap_mut().object_set_prototype(
    class_list_toggle_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(class_list_toggle_func))?;
  let class_list_toggle_key = alloc_key(&mut scope, ELEMENT_CLASS_LIST_TOGGLE_KEY)?;
  scope.define_property(
    document_obj,
    class_list_toggle_key,
    data_desc(Value::Object(class_list_toggle_func)),
  )?;

  let class_list_contains_call_id = vm.register_native_call(element_class_list_contains_native)?;
  let class_list_contains_name = scope.alloc_string("contains")?;
  scope.push_root(Value::String(class_list_contains_name))?;
  let class_list_contains_func = scope.alloc_native_function(
    class_list_contains_call_id,
    None,
    class_list_contains_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    class_list_contains_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(class_list_contains_func))?;
  let class_list_contains_key = alloc_key(&mut scope, ELEMENT_CLASS_LIST_CONTAINS_KEY)?;
  scope.define_property(
    document_obj,
    class_list_contains_key,
    data_desc(Value::Object(class_list_contains_func)),
  )?;

  let class_list_replace_call_id = vm.register_native_call(element_class_list_replace_native)?;
  let class_list_replace_name = scope.alloc_string("replace")?;
  scope.push_root(Value::String(class_list_replace_name))?;
  let class_list_replace_func =
    scope.alloc_native_function(class_list_replace_call_id, None, class_list_replace_name, 2)?;
  scope.heap_mut().object_set_prototype(
    class_list_replace_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(class_list_replace_func))?;
  let class_list_replace_key = alloc_key(&mut scope, ELEMENT_CLASS_LIST_REPLACE_KEY)?;
  scope.define_property(
    document_obj,
    class_list_replace_key,
    data_desc(Value::Object(class_list_replace_func)),
  )?;

  // Store shared Element.id getter/setter functions on `document` so wrappers can reuse them.
  let id_get_call_id = vm.register_native_call(element_id_get_native)?;
  let id_get_name = scope.alloc_string("get id")?;
  scope.push_root(Value::String(id_get_name))?;
  let id_get_func = scope.alloc_native_function(id_get_call_id, None, id_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(id_get_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(id_get_func))?;
  let id_get_key = alloc_key(&mut scope, ELEMENT_ID_GET_KEY)?;
  scope.define_property(
    document_obj,
    id_get_key,
    data_desc(Value::Object(id_get_func)),
  )?;

  let id_set_call_id = vm.register_native_call(element_id_set_native)?;
  let id_set_name = scope.alloc_string("set id")?;
  scope.push_root(Value::String(id_set_name))?;
  let id_set_func = scope.alloc_native_function(id_set_call_id, None, id_set_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(id_set_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(id_set_func))?;
  let id_set_key = alloc_key(&mut scope, ELEMENT_ID_SET_KEY)?;
  scope.define_property(
    document_obj,
    id_set_key,
    data_desc(Value::Object(id_set_func)),
  )?;

  // Store shared reflected attribute accessors on `document` so wrappers can reuse them.
  let reflected_string_get_call_id = vm.register_native_call(element_reflected_string_get_native)?;
  let reflected_string_set_call_id = vm.register_native_call(element_reflected_string_set_native)?;
  let reflected_bool_get_call_id = vm.register_native_call(element_reflected_bool_get_native)?;
  let reflected_bool_set_call_id = vm.register_native_call(element_reflected_bool_set_native)?;

  for (prop, attr, get_key_name, set_key_name) in [
    ("src", "src", ELEMENT_SRC_GET_KEY, ELEMENT_SRC_SET_KEY),
    ("srcset", "srcset", ELEMENT_SRCSET_GET_KEY, ELEMENT_SRCSET_SET_KEY),
    ("sizes", "sizes", ELEMENT_SIZES_GET_KEY, ELEMENT_SIZES_SET_KEY),
    ("href", "href", ELEMENT_HREF_GET_KEY, ELEMENT_HREF_SET_KEY),
    ("rel", "rel", ELEMENT_REL_GET_KEY, ELEMENT_REL_SET_KEY),
    ("type", "type", ELEMENT_TYPE_GET_KEY, ELEMENT_TYPE_SET_KEY),
    ("charset", "charset", ELEMENT_CHARSET_GET_KEY, ELEMENT_CHARSET_SET_KEY),
    (
      "crossOrigin",
      "crossorigin",
      ELEMENT_CROSS_ORIGIN_GET_KEY,
      ELEMENT_CROSS_ORIGIN_SET_KEY,
    ),
    ("height", "height", ELEMENT_HEIGHT_GET_KEY, ELEMENT_HEIGHT_SET_KEY),
    ("width", "width", ELEMENT_WIDTH_GET_KEY, ELEMENT_WIDTH_SET_KEY),
  ] {
    let attr_s = scope.alloc_string(attr)?;
    scope.push_root(Value::String(attr_s))?;

    let get_name = scope.alloc_string(&format!("get {prop}"))?;
    scope.push_root(Value::String(get_name))?;
    let get_func = scope.alloc_native_function_with_slots(
      reflected_string_get_call_id,
      None,
      get_name,
      0,
      &[Value::String(attr_s)],
    )?;
    scope
      .heap_mut()
      .object_set_prototype(get_func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(get_func))?;
    let get_key = alloc_key(&mut scope, get_key_name)?;
    scope.define_property(document_obj, get_key, data_desc(Value::Object(get_func)))?;

    let set_name = scope.alloc_string(&format!("set {prop}"))?;
    scope.push_root(Value::String(set_name))?;
    let set_func = scope.alloc_native_function_with_slots(
      reflected_string_set_call_id,
      None,
      set_name,
      1,
      &[Value::String(attr_s)],
    )?;
    scope
      .heap_mut()
      .object_set_prototype(set_func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(set_func))?;
    let set_key = alloc_key(&mut scope, set_key_name)?;
    scope.define_property(document_obj, set_key, data_desc(Value::Object(set_func)))?;
  }

  for (prop, attr, get_key_name, set_key_name) in [
    ("async", "async", ELEMENT_ASYNC_GET_KEY, ELEMENT_ASYNC_SET_KEY),
    ("defer", "defer", ELEMENT_DEFER_GET_KEY, ELEMENT_DEFER_SET_KEY),
  ] {
    let attr_s = scope.alloc_string(attr)?;
    scope.push_root(Value::String(attr_s))?;

    let get_name = scope.alloc_string(&format!("get {prop}"))?;
    scope.push_root(Value::String(get_name))?;
    let get_func = scope.alloc_native_function_with_slots(
      reflected_bool_get_call_id,
      None,
      get_name,
      0,
      &[Value::String(attr_s)],
    )?;
    scope
      .heap_mut()
      .object_set_prototype(get_func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(get_func))?;
    let get_key = alloc_key(&mut scope, get_key_name)?;
    scope.define_property(document_obj, get_key, data_desc(Value::Object(get_func)))?;

    let set_name = scope.alloc_string(&format!("set {prop}"))?;
    scope.push_root(Value::String(set_name))?;
    let set_func = scope.alloc_native_function_with_slots(
      reflected_bool_set_call_id,
      None,
      set_name,
      1,
      &[Value::String(attr_s)],
    )?;
    scope
      .heap_mut()
      .object_set_prototype(set_func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(set_func))?;
    let set_key = alloc_key(&mut scope, set_key_name)?;
    scope.define_property(document_obj, set_key, data_desc(Value::Object(set_func)))?;
  }

  // Store shared CSSStyleDeclaration methods on `document` so wrappers can reuse them.
  let style_get_property_value_call_id =
    vm.register_native_call(css_style_get_property_value_native)?;
  let style_get_property_value_name = scope.alloc_string("getPropertyValue")?;
  scope.push_root(Value::String(style_get_property_value_name))?;
  let style_get_property_value_func = scope.alloc_native_function(
    style_get_property_value_call_id,
    None,
    style_get_property_value_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    style_get_property_value_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(style_get_property_value_func))?;
  let style_get_property_value_key = alloc_key(&mut scope, STYLE_GET_PROPERTY_VALUE_KEY)?;
  scope.define_property(
    document_obj,
    style_get_property_value_key,
    data_desc(Value::Object(style_get_property_value_func)),
  )?;

  let style_set_property_call_id = vm.register_native_call(css_style_set_property_native)?;
  let style_set_property_name = scope.alloc_string("setProperty")?;
  scope.push_root(Value::String(style_set_property_name))?;
  let style_set_property_func =
    scope.alloc_native_function(style_set_property_call_id, None, style_set_property_name, 2)?;
  scope.heap_mut().object_set_prototype(
    style_set_property_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(style_set_property_func))?;
  let style_set_property_key = alloc_key(&mut scope, STYLE_SET_PROPERTY_KEY)?;
  scope.define_property(
    document_obj,
    style_set_property_key,
    data_desc(Value::Object(style_set_property_func)),
  )?;

  let style_remove_property_call_id = vm.register_native_call(css_style_remove_property_native)?;
  let style_remove_property_name = scope.alloc_string("removeProperty")?;
  scope.push_root(Value::String(style_remove_property_name))?;
  let style_remove_property_func = scope.alloc_native_function(
    style_remove_property_call_id,
    None,
    style_remove_property_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    style_remove_property_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(style_remove_property_func))?;
  let style_remove_property_key = alloc_key(&mut scope, STYLE_REMOVE_PROPERTY_KEY)?;
  scope.define_property(
    document_obj,
    style_remove_property_key,
    data_desc(Value::Object(style_remove_property_func)),
  )?;

  // CSSStyleDeclaration.cssText reflects the element's raw `style` attribute.
  let style_attr_s = scope.alloc_string("style")?;
  scope.push_root(Value::String(style_attr_s))?;
  let css_text_get_name = scope.alloc_string("get cssText")?;
  scope.push_root(Value::String(css_text_get_name))?;
  let css_text_get_func = scope.alloc_native_function_with_slots(
    reflected_string_get_call_id,
    None,
    css_text_get_name,
    0,
    &[Value::String(style_attr_s)],
  )?;
  scope.heap_mut().object_set_prototype(
    css_text_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(css_text_get_func))?;
  let css_text_get_key = alloc_key(&mut scope, STYLE_CSS_TEXT_GET_KEY)?;
  scope.define_property(
    document_obj,
    css_text_get_key,
    data_desc(Value::Object(css_text_get_func)),
  )?;

  let css_text_set_name = scope.alloc_string("set cssText")?;
  scope.push_root(Value::String(css_text_set_name))?;
  let css_text_set_func = scope.alloc_native_function_with_slots(
    reflected_string_set_call_id,
    None,
    css_text_set_name,
    1,
    &[Value::String(style_attr_s)],
  )?;
  scope.heap_mut().object_set_prototype(
    css_text_set_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(css_text_set_func))?;
  let css_text_set_key = alloc_key(&mut scope, STYLE_CSS_TEXT_SET_KEY)?;
  scope.define_property(
    document_obj,
    css_text_set_key,
    data_desc(Value::Object(css_text_set_func)),
  )?;

  let style_named_get_call_id = vm.register_native_call(css_style_named_get_native)?;
  let style_named_set_call_id = vm.register_native_call(css_style_named_set_native)?;

  for (prop, get_key_name, set_key_name) in [
    ("display", STYLE_DISPLAY_GET_KEY, STYLE_DISPLAY_SET_KEY),
    ("cursor", STYLE_CURSOR_GET_KEY, STYLE_CURSOR_SET_KEY),
    ("height", STYLE_HEIGHT_GET_KEY, STYLE_HEIGHT_SET_KEY),
    ("width", STYLE_WIDTH_GET_KEY, STYLE_WIDTH_SET_KEY),
  ] {
    let prop_s = scope.alloc_string(prop)?;
    scope.push_root(Value::String(prop_s))?;

    let get_name = scope.alloc_string(&format!("get {prop}"))?;
    scope.push_root(Value::String(get_name))?;
    let get_func = scope.alloc_native_function_with_slots(
      style_named_get_call_id,
      None,
      get_name,
      0,
      &[Value::String(prop_s)],
    )?;
    scope
      .heap_mut()
      .object_set_prototype(get_func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(get_func))?;
    let get_key = alloc_key(&mut scope, get_key_name)?;
    scope.define_property(document_obj, get_key, data_desc(Value::Object(get_func)))?;

    let set_name = scope.alloc_string(&format!("set {prop}"))?;
    scope.push_root(Value::String(set_name))?;
    let set_func = scope.alloc_native_function_with_slots(
      style_named_set_call_id,
      None,
      set_name,
      1,
      &[Value::String(prop_s)],
    )?;
    scope
      .heap_mut()
      .object_set_prototype(set_func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(set_func))?;
    let set_key = alloc_key(&mut scope, set_key_name)?;
    scope.define_property(document_obj, set_key, data_desc(Value::Object(set_func)))?;
  }

  // Store shared Element.innerHTML/outerHTML accessors on `document` so wrappers can reuse them.
  let inner_html_get_call_id = vm.register_native_call(element_inner_html_get_native)?;
  let inner_html_get_name = scope.alloc_string("get innerHTML")?;
  scope.push_root(Value::String(inner_html_get_name))?;
  let inner_html_get_func =
    scope.alloc_native_function(inner_html_get_call_id, None, inner_html_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    inner_html_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(inner_html_get_func))?;
  let inner_html_get_key = alloc_key(&mut scope, ELEMENT_INNER_HTML_GET_KEY)?;
  scope.define_property(
    document_obj,
    inner_html_get_key,
    data_desc(Value::Object(inner_html_get_func)),
  )?;

  let inner_html_set_call_id = vm.register_native_call(element_inner_html_set_native)?;
  let inner_html_set_name = scope.alloc_string("set innerHTML")?;
  scope.push_root(Value::String(inner_html_set_name))?;
  let inner_html_set_func =
    scope.alloc_native_function(inner_html_set_call_id, None, inner_html_set_name, 1)?;
  scope.heap_mut().object_set_prototype(
    inner_html_set_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(inner_html_set_func))?;
  let inner_html_set_key = alloc_key(&mut scope, ELEMENT_INNER_HTML_SET_KEY)?;
  scope.define_property(
    document_obj,
    inner_html_set_key,
    data_desc(Value::Object(inner_html_set_func)),
  )?;

  let outer_html_get_call_id = vm.register_native_call(element_outer_html_get_native)?;
  let outer_html_get_name = scope.alloc_string("get outerHTML")?;
  scope.push_root(Value::String(outer_html_get_name))?;
  let outer_html_get_func =
    scope.alloc_native_function(outer_html_get_call_id, None, outer_html_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    outer_html_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(outer_html_get_func))?;
  let outer_html_get_key = alloc_key(&mut scope, ELEMENT_OUTER_HTML_GET_KEY)?;
  scope.define_property(
    document_obj,
    outer_html_get_key,
    data_desc(Value::Object(outer_html_get_func)),
  )?;

  let outer_html_set_call_id = vm.register_native_call(element_outer_html_set_native)?;
  let outer_html_set_name = scope.alloc_string("set outerHTML")?;
  scope.push_root(Value::String(outer_html_set_name))?;
  let outer_html_set_func =
    scope.alloc_native_function(outer_html_set_call_id, None, outer_html_set_name, 1)?;
  scope.heap_mut().object_set_prototype(
    outer_html_set_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(outer_html_set_func))?;
  let outer_html_set_key = alloc_key(&mut scope, ELEMENT_OUTER_HTML_SET_KEY)?;
  scope.define_property(
    document_obj,
    outer_html_set_key,
    data_desc(Value::Object(outer_html_set_func)),
  )?;

  // Store shared insertAdjacent* functions.
  let insert_adjacent_html_call_id =
    vm.register_native_call(element_insert_adjacent_html_native)?;
  let insert_adjacent_html_name = scope.alloc_string("insertAdjacentHTML")?;
  scope.push_root(Value::String(insert_adjacent_html_name))?;
  let insert_adjacent_html_func = scope.alloc_native_function(
    insert_adjacent_html_call_id,
    None,
    insert_adjacent_html_name,
    2,
  )?;
  scope.heap_mut().object_set_prototype(
    insert_adjacent_html_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(insert_adjacent_html_func))?;
  let insert_adjacent_html_key = alloc_key(&mut scope, ELEMENT_INSERT_ADJACENT_HTML_KEY)?;
  scope.define_property(
    document_obj,
    insert_adjacent_html_key,
    data_desc(Value::Object(insert_adjacent_html_func)),
  )?;

  let insert_adjacent_element_call_id =
    vm.register_native_call(element_insert_adjacent_element_native)?;
  let insert_adjacent_element_name = scope.alloc_string("insertAdjacentElement")?;
  scope.push_root(Value::String(insert_adjacent_element_name))?;
  let insert_adjacent_element_func = scope.alloc_native_function(
    insert_adjacent_element_call_id,
    None,
    insert_adjacent_element_name,
    2,
  )?;
  scope.heap_mut().object_set_prototype(
    insert_adjacent_element_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(insert_adjacent_element_func))?;
  let insert_adjacent_element_key = alloc_key(&mut scope, ELEMENT_INSERT_ADJACENT_ELEMENT_KEY)?;
  scope.define_property(
    document_obj,
    insert_adjacent_element_key,
    data_desc(Value::Object(insert_adjacent_element_func)),
  )?;

  let insert_adjacent_text_call_id =
    vm.register_native_call(element_insert_adjacent_text_native)?;
  let insert_adjacent_text_name = scope.alloc_string("insertAdjacentText")?;
  scope.push_root(Value::String(insert_adjacent_text_name))?;
  let insert_adjacent_text_func = scope.alloc_native_function(
    insert_adjacent_text_call_id,
    None,
    insert_adjacent_text_name,
    2,
  )?;
  scope.heap_mut().object_set_prototype(
    insert_adjacent_text_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(insert_adjacent_text_func))?;
  let insert_adjacent_text_key = alloc_key(&mut scope, ELEMENT_INSERT_ADJACENT_TEXT_KEY)?;
  scope.define_property(
    document_obj,
    insert_adjacent_text_key,
    data_desc(Value::Object(insert_adjacent_text_func)),
  )?;

  let current_script_guard = config
    .current_script_state
    .clone()
    .map(CurrentScriptSourceGuard::new);
  if let Some(guard) = current_script_guard.as_ref() {
    let id_key = alloc_key(&mut scope, CURRENT_SCRIPT_SOURCE_ID_KEY)?;
    scope.define_property(
      document_obj,
      id_key,
      data_desc(Value::Number(guard.id() as f64)),
    )?;
  }
  scope.define_property(
    document_obj,
    current_script_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(current_script_func),
        set: Value::Undefined,
      },
    },
  )?;

  // document.cookie
  let cookie_key = alloc_key(&mut scope, "cookie")?;
  let cookie_get_call_id = vm.register_native_call(document_cookie_get_native)?;
  let cookie_get_name = scope.alloc_string("get cookie")?;
  scope.push_root(Value::String(cookie_get_name))?;
  let cookie_get_func =
    scope.alloc_native_function(cookie_get_call_id, None, cookie_get_name, 0)?;
  scope.heap_mut().object_set_prototype(
    cookie_get_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(cookie_get_func))?;

  let cookie_set_call_id = vm.register_native_call(document_cookie_set_native)?;
  let cookie_set_name = scope.alloc_string("set cookie")?;
  scope.push_root(Value::String(cookie_set_name))?;
  let cookie_set_func =
    scope.alloc_native_function(cookie_set_call_id, None, cookie_set_name, 1)?;
  scope.heap_mut().object_set_prototype(
    cookie_set_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(cookie_set_func))?;

  scope.define_property(
    document_obj,
    cookie_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(cookie_get_func),
        set: Value::Object(cookie_set_func),
      },
    },
  )?;

  let console_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(console_obj))?;
  let console_call_id = vm.register_native_call(console_call_native)?;
  let sink_id_key_s = scope.alloc_string(CONSOLE_SINK_ID_KEY)?;
  scope.push_root(Value::String(sink_id_key_s))?;

  let define_console_method =
    |scope: &mut Scope<'_>, name: &str, level: ConsoleMessageLevel| -> Result<Value, VmError> {
    let level_slot = Value::Number(match level {
      ConsoleMessageLevel::Log => 0.0,
      ConsoleMessageLevel::Info => 1.0,
      ConsoleMessageLevel::Warn => 2.0,
      ConsoleMessageLevel::Error => 3.0,
      ConsoleMessageLevel::Debug => 4.0,
    });

    let slots = [
      level_slot,
      Value::Object(console_obj),
      Value::String(sink_id_key_s),
    ];

    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function_with_slots(console_call_id, None, name_s, 0, &slots)?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(func))?;
    Ok(Value::Object(func))
  };

  let log_key = alloc_key(&mut scope, "log")?;
  let info_key = alloc_key(&mut scope, "info")?;
  let warn_key = alloc_key(&mut scope, "warn")?;
  let error_key = alloc_key(&mut scope, "error")?;
  let debug_key = alloc_key(&mut scope, "debug")?;

  let log_func = define_console_method(&mut scope, "log", ConsoleMessageLevel::Log)?;
  let info_func = define_console_method(&mut scope, "info", ConsoleMessageLevel::Info)?;
  let warn_func = define_console_method(&mut scope, "warn", ConsoleMessageLevel::Warn)?;
  let error_func = define_console_method(&mut scope, "error", ConsoleMessageLevel::Error)?;
  let debug_func = define_console_method(&mut scope, "debug", ConsoleMessageLevel::Debug)?;

  scope.define_property(console_obj, log_key, data_desc(log_func))?;
  scope.define_property(console_obj, info_key, data_desc(info_func))?;
  scope.define_property(console_obj, warn_key, data_desc(warn_func))?;
  scope.define_property(console_obj, error_key, data_desc(error_func))?;
  scope.define_property(console_obj, debug_key, data_desc(debug_func))?;

  let console_sink_guard = config.console_sink.clone().map(ConsoleSinkGuard::new);
  if let Some(guard) = console_sink_guard.as_ref() {
    let sink_key = PropertyKey::from_string(sink_id_key_s);
    scope.define_property(
      console_obj,
      sink_key,
      data_desc(Value::Number(guard.id() as f64)),
    )?;
  }

  scope.define_property(global, global_this_key, data_desc(Value::Object(global)))?;
  scope.define_property(global, window_key, data_desc(Value::Object(global)))?;
  scope.define_property(global, self_key, data_desc(Value::Object(global)))?;
  scope.define_property(global, top_key, data_desc(Value::Object(global)))?;
  scope.define_property(global, parent_key, data_desc(Value::Object(global)))?;

  scope.define_property(
    global,
    location_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(window_location_get_func),
        set: Value::Object(window_location_set_func),
      },
    },
  )?;
  scope.define_property(global, document_key, data_desc(Value::Object(document_obj)))?;
  scope.define_property(global, console_key, data_desc(Value::Object(console_obj)))?;

  // Cache stable window/document handles for event dispatch in non-DOM-backed realms and for
  // `new EventTarget()` instances.
  if let Some(user_data) = vm.user_data_mut::<WindowRealmUserData>() {
    user_data.window_obj = Some(global);
    user_data.document_obj = Some(document_obj);
  }

  // --- Web Storage (localStorage / sessionStorage) ---------------------------
  //
  // Many real-world pages (including MDN + news sites) read from `localStorage` early to decide
  // theme/layout. Provide a minimal, deterministic `Storage` facade backed by a plain object map.
  let storage_length_get_call_id = vm.register_native_call(storage_length_get_native)?;
  let storage_get_item_call_id = vm.register_native_call(storage_get_item_native)?;
  let storage_set_item_call_id = vm.register_native_call(storage_set_item_native)?;
  let storage_remove_item_call_id = vm.register_native_call(storage_remove_item_native)?;
  let storage_clear_call_id = vm.register_native_call(storage_clear_native)?;
  let storage_key_call_id = vm.register_native_call(storage_key_native)?;

  let storage_length_key = alloc_key(&mut scope, "length")?;
  let storage_get_item_key = alloc_key(&mut scope, "getItem")?;
  let storage_set_item_key = alloc_key(&mut scope, "setItem")?;
  let storage_remove_item_key = alloc_key(&mut scope, "removeItem")?;
  let storage_clear_key = alloc_key(&mut scope, "clear")?;
  let storage_key_key = alloc_key(&mut scope, "key")?;

  install_storage_object(
    vm,
    &mut scope,
    realm,
    global,
    local_storage_key,
    "localStorage",
    storage_length_get_call_id,
    storage_get_item_call_id,
    storage_set_item_call_id,
    storage_remove_item_call_id,
    storage_clear_call_id,
    storage_key_call_id,
    storage_length_key,
    storage_get_item_key,
    storage_set_item_key,
    storage_remove_item_key,
    storage_clear_key,
    storage_key_key,
  )?;
  install_storage_object(
    vm,
    &mut scope,
    realm,
    global,
    session_storage_key,
    "sessionStorage",
    storage_length_get_call_id,
    storage_get_item_call_id,
    storage_set_item_call_id,
    storage_remove_item_call_id,
    storage_clear_call_id,
    storage_key_call_id,
    storage_length_key,
    storage_get_item_key,
    storage_set_item_key,
    storage_remove_item_key,
    storage_clear_key,
    storage_key_key,
  )?;

  // --- WindowOrWorkerGlobalScope primitives ---------------------------------
  //
  // These are frequently used by real-world scripts (`atob`/`btoa` especially) and should be
  // installed early to avoid brittle failures.
  let window_origin_key = alloc_key(&mut scope, "origin")?;
  scope.define_property(
    global,
    window_origin_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: origin_v,
        writable: false,
      },
    },
  )?;

  let is_secure_context_key = alloc_key(&mut scope, "isSecureContext")?;
  scope.define_property(
    global,
    is_secure_context_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: is_secure_context_v,
        writable: false,
      },
    },
  )?;

  let cross_origin_isolated_key = alloc_key(&mut scope, "crossOriginIsolated")?;
  scope.define_property(
    global,
    cross_origin_isolated_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: cross_origin_isolated_v,
        writable: false,
      },
    },
  )?;

  // atob(data)
  let atob_call_id = vm.register_native_call(window_atob_native)?;
  let atob_name = scope.alloc_string("atob")?;
  scope.push_root(Value::String(atob_name))?;
  let atob_func = scope.alloc_native_function(atob_call_id, None, atob_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(atob_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(atob_func))?;
  let atob_key = alloc_key(&mut scope, "atob")?;
  scope.define_property(global, atob_key, data_desc(Value::Object(atob_func)))?;

  // btoa(data)
  let btoa_call_id = vm.register_native_call(window_btoa_native)?;
  let btoa_name = scope.alloc_string("btoa")?;
  scope.push_root(Value::String(btoa_name))?;
  let btoa_func = scope.alloc_native_function(btoa_call_id, None, btoa_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(btoa_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(btoa_func))?;
  let btoa_key = alloc_key(&mut scope, "btoa")?;
  scope.define_property(global, btoa_key, data_desc(Value::Object(btoa_func)))?;

  // reportError(e)
  let report_error_call_id = vm.register_native_call(window_report_error_native)?;
  let report_error_name = scope.alloc_string("reportError")?;
  scope.push_root(Value::String(report_error_name))?;
  let report_error_slots = [console_sink_guard
    .as_ref()
    .map(|guard| Value::Number(guard.id() as f64))
    .unwrap_or(Value::Undefined)];
  let report_error_func = scope.alloc_native_function_with_slots(
    report_error_call_id,
    None,
    report_error_name,
    1,
    &report_error_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    report_error_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(report_error_func))?;
  let report_error_key = alloc_key(&mut scope, "reportError")?;
  scope.define_property(
    global,
    report_error_key,
    data_desc(Value::Object(report_error_func)),
  )?;

  // --- Deterministic browser environment shims ---------------------------------
  //
  // Real-world scripts often gate responsive logic on these.
  let window_env = WindowEnv::from_media(config.media.clone());
  let match_media_guard = MatchMediaEnvGuard::new(window_env.media.clone());
  install_window_shims_vm_js(
    vm,
    &mut scope,
    realm,
    global,
    window_env,
    match_media_guard.id(),
  )?;

  if let Some(platform) = dom_platform.take() {
    if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
      data.dom_platform = Some(platform);
    }
  }

  // Install WHATWG URL bindings (`URL`/`URLSearchParams`) so real-world scripts can parse and
  // manipulate URLs. This must happen after `scope` is dropped because it borrows `heap` mutably.
  drop(scope);
  crate::js::window_abort::install_window_abort_bindings(vm, realm, heap)?;
  crate::js::window_text_encoding::install_window_text_encoding_bindings(vm, realm, heap)?;
  crate::js::window_url::install_window_url_bindings(vm, realm, heap)?;

  Ok((
    console_sink_guard.map(ConsoleSinkGuard::disarm),
    current_script_guard.map(CurrentScriptSourceGuard::disarm),
    Some(match_media_guard.disarm()),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::window_env::FASTRENDER_USER_AGENT;
  use crate::js::clock::VirtualClock;
  use std::ptr::NonNull;
  use std::sync::{Mutex as StdMutex, OnceLock as StdOnceLock};
  use std::time::Duration;

  #[derive(Debug, Clone, PartialEq)]
  enum CapturedConsoleArg {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    BigInt(String),
    String(String),
    Object,
    Symbol,
  }

  #[derive(Debug, Clone, PartialEq)]
  struct CapturedConsoleCall {
    level: ConsoleMessageLevel,
    args: Vec<CapturedConsoleArg>,
  }

  #[derive(Default)]
  struct NoopHostHooks;

  impl vm_js::VmHostHooks for NoopHostHooks {
    fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {}
  }

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  fn get_prop(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    obj: GcObject,
    name: &str,
  ) -> Result<Value, VmError> {
    // Root the object while allocating the property key: string allocation can trigger GC.
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(obj))?;
    let key = alloc_key(&mut scope, name)?;
    vm.get(&mut scope, obj, key)
  }

  fn unwrap_thrown_object(err: VmError) -> GcObject {
    match err.thrown_value() {
      Some(Value::Object(obj)) => obj,
      Some(other) => panic!("expected thrown object, got {other:?}"),
      None => panic!("expected thrown error, got {err:?}"),
    }
  }

  fn console_sink_test_lock() -> &'static StdMutex<()> {
    static LOCK: StdOnceLock<StdMutex<()>> = StdOnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
  }

  struct DomSourceGuard {
    id: u64,
  }

  impl Drop for DomSourceGuard {
    fn drop(&mut self) {
      unregister_dom_source(self.id);
    }
  }

  fn new_realm(config: WindowRealmConfig) -> Result<WindowRealm, VmError> {
    let mut js_execution_options = JsExecutionOptions::default();
    // These unit tests validate DOM/Web API behaviour, not the per-run wall time budget. Increase
    // it so debug builds running tests in parallel don't trip the default 100ms budget.
    js_execution_options.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(1));
    // Keep the heap limits configured by `WindowRealmConfig` (some tests tweak it).
    js_execution_options.max_vm_heap_bytes = None;
    WindowRealm::new_with_js_execution_options(config, js_execution_options)
  }

  #[test]
  fn window_env_shims_exist_and_match_media_evaluates() -> Result<(), VmError> {
    let media = MediaContext::screen(800.0, 600.0).with_device_pixel_ratio(2.0);
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/").with_media_context(media))?;

    let dpr = realm.exec_script("devicePixelRatio")?;
    assert!(matches!(dpr, Value::Number(v) if (v - 2.0).abs() < f64::EPSILON));

    let ua = realm.exec_script("navigator.userAgent")?;
    assert_eq!(get_string(realm.heap(), ua), FASTRENDER_USER_AGENT);

    assert_eq!(
      realm.exec_script("matchMedia('(min-width: 700px)').matches")?,
      Value::Bool(true)
    );
    assert_eq!(
      realm.exec_script("matchMedia('(min-resolution: 2dppx)').matches")?,
      Value::Bool(true)
    );
    assert_eq!(
      realm.exec_script("matchMedia('(max-resolution: 1.5dppx)').matches")?,
      Value::Bool(false)
    );

    // Listener APIs are allowed to be stubs, but they must exist and be callable.
    assert_eq!(
      realm.exec_script("{ const m = matchMedia('(min-width: 1px)'); m.addListener(()=>{}); m.removeListener(()=>{}); true }")?,
      Value::Bool(true)
    );

    Ok(())
  }

  #[test]
  fn window_text_encoding_exists_and_round_trips() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let decoded = realm.exec_script("new TextDecoder().decode(new TextEncoder().encode('hi'))")?;
    assert_eq!(get_string(realm.heap(), decoded), "hi");
    Ok(())
  }

  #[test]
  fn window_storage_exists_and_round_trips() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    assert_eq!(
      realm.exec_script("localStorage.getItem('missing')")?,
      Value::Null
    );

    realm.exec_script("localStorage.setItem('a', 1)")?;
    let a = realm.exec_script("localStorage.getItem('a')")?;
    assert_eq!(get_string(realm.heap(), a), "1");
    assert!(matches!(
      realm.exec_script("localStorage.length")?,
      Value::Number(n) if n == 1.0
    ));

    realm.exec_script("localStorage.setItem('b', '2')")?;
    assert!(matches!(
      realm.exec_script("localStorage.length")?,
      Value::Number(n) if n == 2.0
    ));

    let key0 = realm.exec_script("localStorage.key(0)")?;
    assert_eq!(get_string(realm.heap(), key0), "a");
    let key1 = realm.exec_script("localStorage.key(1)")?;
    assert_eq!(get_string(realm.heap(), key1), "b");
    assert_eq!(realm.exec_script("localStorage.key(2)")?, Value::Null);

    realm.exec_script("localStorage.removeItem('a')")?;
    assert_eq!(realm.exec_script("localStorage.getItem('a')")?, Value::Null);
    assert!(matches!(
      realm.exec_script("localStorage.length")?,
      Value::Number(n) if n == 1.0
    ));

    realm.exec_script("localStorage.clear()")?;
    assert!(matches!(
      realm.exec_script("localStorage.length")?,
      Value::Number(n) if n == 0.0
    ));

    // sessionStorage should be isolated from localStorage.
    realm.exec_script("sessionStorage.setItem('x', 's'); localStorage.setItem('x', 'l');")?;
    let s = realm.exec_script("sessionStorage.getItem('x')")?;
    assert_eq!(get_string(realm.heap(), s), "s");
    let l = realm.exec_script("localStorage.getItem('x')")?;
    assert_eq!(get_string(realm.heap(), l), "l");

    Ok(())
  }

  #[test]
  fn url_objects_coerce_via_string_constructor_and_to_string() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    // `String(urlObj)` should invoke `ToString`, which for objects uses `ToPrimitive` and then
    // calls the URL wrapper's `toString()` method.
    let s = realm.exec_script("String(new URL('https://example.com/'))")?;
    assert_eq!(get_string(realm.heap(), s), "https://example.com/");

    // `new URL(rel, baseUrlObj)` should accept a URL object as the base (via `ToString(base)`).
    let href = realm.exec_script(
      "(() => {\n\
        const base = new URL('https://example.com/dir/');\n\
        const u = new URL('file', base);\n\
        return u.href;\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), href), "https://example.com/dir/file");

    Ok(())
  }

  #[test]
  fn time_bindings_survive_windowrealm_moves() -> Result<(), VmError> {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_realm: Arc<dyn Clock> = clock.clone();
    let web_time = WebTime::new(0);
    let realm = new_realm(
      WindowRealmConfig::new("https://example.com/")
        .with_clock(clock_for_realm)
        .with_web_time(web_time),
    )?;

    let mut realms = Vec::new();
    realms.push(realm);
    let mut realm = realms.pop().expect("expected a moved realm");

    clock.set_now(Duration::from_millis(5));
    assert_eq!(realm.exec_script("performance.now()")?, Value::Number(5.0));
    Ok(())
  }

  #[test]
  fn time_bindings_exist_and_follow_configured_clock() -> Result<(), VmError> {
    let clock = Arc::new(VirtualClock::new());
    let clock_for_realm: Arc<dyn Clock> = clock.clone();
    let web_time = WebTime::new(1_000);
    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/")
        .with_clock(clock_for_realm)
        .with_web_time(web_time),
    )?;

    clock.set_now(Duration::from_millis(0));
    assert_eq!(realm.exec_script("typeof Date === 'function'")?, Value::Bool(true));
    assert_eq!(realm.exec_script("typeof Date.now === 'function'")?, Value::Bool(true));
    assert_eq!(realm.exec_script("new Date(123).getTime()")?, Value::Number(123.0));
    assert_eq!(realm.exec_script("new Date().getTime()")?, Value::Number(1_000.0));
    assert_eq!(
      realm.exec_script("performance.timeOrigin")?,
      Value::Number(web_time.time_origin_unix_ms as f64)
    );
    assert_eq!(realm.exec_script("Date.now()")?, Value::Number(1_000.0));
    assert_eq!(realm.exec_script("performance.now()")?, Value::Number(0.0));

    // Advance to a deterministic non-integer millisecond.
    clock.set_now(Duration::from_nanos(1_234_567_890)); // 1234.56789ms
    assert_eq!(realm.exec_script("Date.now()")?, Value::Number(2_234.0));
    assert_eq!(realm.exec_script("new Date().getTime()")?, Value::Number(2_234.0));
    let Value::Number(perf_now) = realm.exec_script("performance.now()")? else {
      panic!("expected performance.now() to return a number");
    };
    assert!((perf_now - 1234.56789).abs() < 1e-9);

    Ok(())
  }

  #[test]
  fn document_state_properties_exist() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ready_state = realm.exec_script("document.readyState")?;
    assert_eq!(get_string(realm.heap(), ready_state), "complete");

    let visibility = realm.exec_script("document.visibilityState")?;
    assert_eq!(get_string(realm.heap(), visibility), "visible");

    assert_eq!(realm.exec_script("document.hidden")?, Value::Bool(false));

    Ok(())
  }

  #[test]
  fn document_ready_state_reflects_dom2_document_ready_state() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    for (state, expected) in [
      (crate::web::dom::DocumentReadyState::Loading, "loading"),
      (crate::web::dom::DocumentReadyState::Interactive, "interactive"),
      (crate::web::dom::DocumentReadyState::Complete, "complete"),
    ] {
      dom.set_ready_state(state);
      let ready_state = realm.exec_script("document.readyState")?;
      assert_eq!(get_string(realm.heap(), ready_state), expected);
    }

    dom.set_ready_state(crate::web::dom::DocumentReadyState::Loading);
    realm.exec_script("document.readyState = 'complete'")?;
    let ready_state = realm.exec_script("document.readyState")?;
    assert_eq!(get_string(realm.heap(), ready_state), "loading");

    Ok(())
  }

  #[test]
  fn document_referrer_exists_and_defaults_to_empty_string() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let referrer = realm.exec_script("document.referrer")?;
    assert_eq!(get_string(realm.heap(), referrer), "");

    Ok(())
  }

  #[test]
  fn domless_window_and_document_event_targets_dispatch_via_fallback_registry() -> Result<(), VmError>
  {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let order = realm.exec_script(
      "(() => {\n\
        let log = '';\n\
        const push = (s) => { log = log === '' ? s : log + ',' + s; };\n\
        window.addEventListener('x', () => push('w-c'), true);\n\
        window.addEventListener('x', () => push('w-b'));\n\
        document.addEventListener('x', () => push('d-c'), { capture: true });\n\
        document.addEventListener('x', () => push('d-b'));\n\
\n\
        document.dispatchEvent(new Event('x'));\n\
        const first = log;\n\
        log = '';\n\
        document.dispatchEvent(new Event('x', { bubbles: true }));\n\
        const second = log;\n\
        return first + '|' + second;\n\
      })()",
    )?;
    assert_eq!(
      get_string(realm.heap(), order),
      "w-c,d-c,d-b|w-c,d-c,d-b,w-b"
    );
    Ok(())
  }

  #[test]
  fn event_target_constructor_dispatches_via_shared_event_system() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    let called = realm.exec_script(
      "(() => {\n\
        const t = new EventTarget();\n\
        let count = 0;\n\
        t.addEventListener('x', () => { count++; });\n\
        t.dispatchEvent(new Event('x'));\n\
        return count;\n\
      })()",
    )?;
    assert_eq!(called, Value::Number(1.0));

    let removed = realm.exec_script(
      "(() => {\n\
        const t = new EventTarget();\n\
        let count = 0;\n\
        const cb = () => { count++; };\n\
        t.addEventListener('x', cb);\n\
        t.removeEventListener('x', cb);\n\
        t.dispatchEvent(new Event('x'));\n\
        return count;\n\
      })()",
    )?;
    assert_eq!(removed, Value::Number(0.0));
    Ok(())
  }

  #[test]
  fn dom_event_listeners_are_registered_in_dom2_and_invoked_by_host_dispatch() -> Result<(), VmError>
  {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    realm.exec_script(
      "globalThis.__count = 0;\n\
       document.addEventListener('x', () => { __count++; });\n\
       globalThis.__ev = new Event('x');",
    )?;

    let unbound_error = realm.exec_script(
      "(() => {\n\
        const add = document.addEventListener;\n\
        add('x', () => {});\n\
      })()",
    );
    let err = unbound_error.expect_err("expected unbound addEventListener to throw");
    match err {
      VmError::TypeError(msg) => assert_eq!(msg, "Illegal invocation"),
      other => {
        let obj = unwrap_thrown_object(other);
        let (vm, heap) = realm.vm_and_heap_mut();
        let mut scope = heap.scope();
        scope.push_root(Value::Object(obj))?;
        let name = get_prop(vm, &mut scope, obj, "name")?;
        assert_eq!(get_string(scope.heap(), name), "TypeError");
        let message = get_prop(vm, &mut scope, obj, "message")?;
        assert_eq!(get_string(scope.heap(), message), "Illegal invocation");
      }
    }

    let default_not_prevented = {
      let realm_id = realm.realm_id;
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
        realm: realm_id,
        script_or_module: None,
      });

      let global = realm_ref.global_object();
      let document_obj = match get_prop(&mut vm, &mut scope, global, "document")? {
        Value::Object(obj) => obj,
        other => panic!("expected document object, got {other:?}"),
      };
      let event_obj = match get_prop(&mut vm, &mut scope, global, "__ev")? {
        Value::Object(obj) => obj,
        other => panic!("expected Event object, got {other:?}"),
      };
      let listener_roots = super::get_or_create_event_listener_roots(&mut scope, document_obj)?;

      let mut hooks = NoopHostHooks::default();
      let mut host_ctx = ();
      let mut invoker = super::VmJsDomEventInvoker {
        vm: &mut *vm,
        scope: &mut scope,
        host: &mut host_ctx,
        hooks: &mut hooks,
        window_obj: global,
        document_obj,
        event_obj,
        listener_roots_owner: document_obj,
        listener_roots,
        opaque_target_obj: None,
        registry: dom.events(),
      };

      let mut event = web_events::Event::new(
        "x",
        web_events::EventInit {
          bubbles: false,
          cancelable: false,
          composed: false,
        },
      );
      web_events::dispatch_event(
        web_events::EventTargetId::Document,
        &mut event,
        dom.as_ref(),
        dom.events(),
        &mut invoker,
      )
      .expect("dispatch_event should succeed")
    };

    assert_eq!(default_not_prevented, true);
    assert_eq!(realm.exec_script("__count")?, Value::Number(1.0));
    Ok(())
  }

  #[test]
  fn dom_event_once_listeners_are_removed_after_first_dispatch() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    realm.exec_script(
      "globalThis.__count = 0;\n\
       document.addEventListener('x', () => { __count++; }, { once: true });\n\
       globalThis.__ev = new Event('x');",
    )?;

    for _ in 0..2 {
      let realm_id = realm.realm_id;
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
        realm: realm_id,
        script_or_module: None,
      });
      let global = realm_ref.global_object();
      let document_obj = match get_prop(&mut vm, &mut scope, global, "document")? {
        Value::Object(obj) => obj,
        other => panic!("expected document object, got {other:?}"),
      };
      let event_obj = match get_prop(&mut vm, &mut scope, global, "__ev")? {
        Value::Object(obj) => obj,
        other => panic!("expected Event object, got {other:?}"),
      };
      let listener_roots = super::get_or_create_event_listener_roots(&mut scope, document_obj)?;
      let mut hooks = NoopHostHooks::default();
      let mut host_ctx = ();
      let mut invoker = super::VmJsDomEventInvoker {
        vm: &mut *vm,
        scope: &mut scope,
        host: &mut host_ctx,
        hooks: &mut hooks,
        window_obj: global,
        document_obj,
        event_obj,
        listener_roots_owner: document_obj,
        listener_roots,
        opaque_target_obj: None,
        registry: dom.events(),
      };

      let mut event = web_events::Event::new(
        "x",
        web_events::EventInit {
          bubbles: false,
          cancelable: false,
          composed: false,
        },
      );
      web_events::dispatch_event(
        web_events::EventTargetId::Document,
        &mut event,
        dom.as_ref(),
        dom.events(),
        &mut invoker,
      )
      .expect("dispatch_event should succeed");
    }

    assert_eq!(realm.exec_script("__count")?, Value::Number(1.0));
    Ok(())
  }

  #[test]
  fn dom_event_stop_immediate_propagation_stops_later_listeners() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    realm.exec_script(
      "globalThis.__count = 0;\n\
       document.addEventListener('x', (e) => { __count++; e.stopImmediatePropagation(); });\n\
       document.addEventListener('x', () => { __count++; });\n\
       globalThis.__ev = new Event('x');",
    )?;

    {
      let realm_id = realm.realm_id;
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
        realm: realm_id,
        script_or_module: None,
      });
      let global = realm_ref.global_object();
      let document_obj = match get_prop(&mut vm, &mut scope, global, "document")? {
        Value::Object(obj) => obj,
        other => panic!("expected document object, got {other:?}"),
      };
      let event_obj = match get_prop(&mut vm, &mut scope, global, "__ev")? {
        Value::Object(obj) => obj,
        other => panic!("expected Event object, got {other:?}"),
      };
      let listener_roots = super::get_or_create_event_listener_roots(&mut scope, document_obj)?;
      let mut hooks = NoopHostHooks::default();
      let mut host_ctx = ();
      let mut invoker = super::VmJsDomEventInvoker {
        vm: &mut *vm,
        scope: &mut scope,
        host: &mut host_ctx,
        hooks: &mut hooks,
        window_obj: global,
        document_obj,
        event_obj,
        listener_roots_owner: document_obj,
        listener_roots,
        opaque_target_obj: None,
        registry: dom.events(),
      };
      let mut event = web_events::Event::new(
        "x",
        web_events::EventInit {
          bubbles: false,
          cancelable: false,
          composed: false,
        },
      );
      web_events::dispatch_event(
        web_events::EventTargetId::Document,
        &mut event,
        dom.as_ref(),
        dom.events(),
        &mut invoker,
      )
      .expect("dispatch_event should succeed");
    }
    assert_eq!(realm.exec_script("__count")?, Value::Number(1.0));
    Ok(())
  }

  #[test]
  fn event_target_listeners_can_be_registered_and_dispatched() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut _dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(_dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    // Window add/remove/dispatch plumbing.
    let called = realm.exec_script(
      "(() => {\n\
        let count = 0;\n\
        function cb(e) { if (e.type === 'ping') count++; }\n\
        addEventListener('ping', cb);\n\
        dispatchEvent(new Event('ping'));\n\
        removeEventListener('ping', cb);\n\
        dispatchEvent(new Event('ping'));\n\
        return count;\n\
      })()",
    )?;
    assert_eq!(called, Value::Number(1.0));

    // `dispatchEvent` return value reflects `preventDefault()` for cancelable events.
    let prevented = realm.exec_script(
      "(() => {\n\
        function cb(e) { e.preventDefault(); }\n\
        addEventListener('p', cb);\n\
        const ev = new Event('p', { cancelable: true });\n\
        return dispatchEvent(ev);\n\
      })()",
    )?;
    assert_eq!(prevented, Value::Bool(false));

    // Document dispatch also works and can use CustomEvent.
    let doc_called = realm.exec_script(
      "(() => {\n\
        let count = 0;\n\
        document.addEventListener('x', () => { count++; });\n\
        document.dispatchEvent(new CustomEvent('x'));\n\
        return count;\n\
      })()",
    )?;
    assert_eq!(doc_called, Value::Number(1.0));

    Ok(())
  }

  #[test]
  fn event_target_constructor_exists_and_dispatches() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut _dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(_dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        if (typeof EventTarget !== 'function') return false;\n\
        if (typeof EventTarget.prototype.addEventListener !== 'function') return false;\n\
        const et = new EventTarget();\n\
        let count = 0;\n\
        et.addEventListener('x', () => { count++; });\n\
        et.dispatchEvent(new Event('x'));\n\
        return count === 1;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn event_listener_this_and_targets_are_set() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut _dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(_dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const et = new EventTarget();\n\
        let ok = true;\n\
        let called = false;\n\
        function cb(e) {\n\
          called = true;\n\
          ok = ok && (this === et);\n\
          ok = ok && (e.target === et);\n\
          ok = ok && (e.currentTarget === et);\n\
        }\n\
        et.addEventListener('x', cb);\n\
        et.dispatchEvent(new Event('x'));\n\
        return ok && called;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn stop_immediate_propagation_stops_subsequent_listeners() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut _dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(_dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let result = realm.exec_script(
      "(() => {\n\
        const et = new EventTarget();\n\
        let log = '';\n\
        et.addEventListener('x', (e) => { log += 'a'; e.stopImmediatePropagation(); });\n\
        et.addEventListener('x', () => { log += 'b'; });\n\
        et.dispatchEvent(new Event('x'));\n\
        return log;\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), result), "a");
    Ok(())
  }

  #[test]
  fn add_event_listener_dedupes_by_callback_and_capture() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut _dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(_dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let count = realm.exec_script(
      "(() => {\n\
        const et = new EventTarget();\n\
        let count = 0;\n\
        function cb() { count++; }\n\
        et.addEventListener('x', cb);\n\
        et.addEventListener('x', cb);\n\
        et.dispatchEvent(new Event('x'));\n\
        et.addEventListener('x', cb, true);\n\
        et.addEventListener('x', cb, true);\n\
        et.dispatchEvent(new Event('x'));\n\
        return count;\n\
      })()",
    )?;
    assert_eq!(count, Value::Number(3.0));
    Ok(())
  }

  #[test]
  fn dom_event_prevent_default_affects_dispatch_event_return_value() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    realm.exec_script(
      "document.addEventListener('x', (e) => { e.preventDefault(); });\n\
       globalThis.__ev = new Event('x', { cancelable: true });",
    )?;

    let realm_id = realm.realm_id;
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
      realm: realm_id,
      script_or_module: None,
    });
    let global = realm_ref.global_object();
    let document_obj = match get_prop(&mut vm, &mut scope, global, "document")? {
      Value::Object(obj) => obj,
      other => panic!("expected document object, got {other:?}"),
    };
    let event_obj = match get_prop(&mut vm, &mut scope, global, "__ev")? {
      Value::Object(obj) => obj,
      other => panic!("expected Event object, got {other:?}"),
    };
    let listener_roots = super::get_or_create_event_listener_roots(&mut scope, document_obj)?;

    let mut hooks = NoopHostHooks::default();
    let mut host_ctx = ();
    let mut invoker = super::VmJsDomEventInvoker {
      vm: &mut *vm,
      scope: &mut scope,
      host: &mut host_ctx,
      hooks: &mut hooks,
      window_obj: global,
      document_obj,
      event_obj,
      listener_roots_owner: document_obj,
      listener_roots,
      opaque_target_obj: None,
      registry: dom.events(),
    };

    let mut event = web_events::Event::new(
      "x",
      web_events::EventInit {
        bubbles: false,
        cancelable: true,
        composed: false,
      },
    );
    let default_not_prevented = web_events::dispatch_event(
      web_events::EventTargetId::Document,
      &mut event,
      dom.as_ref(),
      dom.events(),
      &mut invoker,
    )
    .expect("dispatch_event should succeed");
    assert_eq!(default_not_prevented, false);
    Ok(())
  }

  #[test]
  fn node_wrappers_expose_event_target_methods() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let called = realm.exec_script(
      "(() => {\n\
        let count = 0;\n\
        const el = document.createElement('div');\n\
        el.addEventListener('hi', () => { count++; });\n\
        el.dispatchEvent(new Event('hi'));\n\
        return count;\n\
      })()",
    )?;
    assert_eq!(called, Value::Number(1.0));
    Ok(())
  }

  #[test]
  fn document_element_class_name_mutates_dom2_document() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;
    realm.exec_script("document.documentElement.className = 'hello'")?;

    let doc_el = dom
      .document_element()
      .expect("document element should exist");
    assert_eq!(dom.element_class_name(doc_el), "hello");
    Ok(())
  }

  #[test]
  fn element_class_list_mutates_dom2_document() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=target class=\"a b\"></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const el = document.getElementById('target');\n\
        el.classList.add('c', 'd');\n\
        const hasB = el.classList.contains('b');\n\
        el.classList.remove('a');\n\
        const toggled = el.classList.toggle('e');\n\
        const replaced = el.classList.replace('b', 'f');\n\
        let err = 'no';\n\
        try { el.classList.add(''); } catch (e) { err = e.name; }\n\
        return hasB && toggled && replaced && err === 'SyntaxError' && el.className === 'f c d e';\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let target = dom.get_element_by_id("target").expect("missing #target");
    assert_eq!(dom.element_class_name(target), "f c d e");
    Ok(())
  }

  #[test]
  fn element_reflected_attributes_mutate_dom2_document() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const script = document.createElement('script');\n\
        script.id = 's';\n\
        script.src = 'https://example.com/app.js';\n\
        script.type = 'module';\n\
        script.charset = 'utf-8';\n\
        script.crossOrigin = 'anonymous';\n\
        script.async = true;\n\
        script.defer = true;\n\
        document.body.appendChild(script);\n\
        return script.src === 'https://example.com/app.js'\n\
          && script.type === 'module'\n\
          && script.charset === 'utf-8'\n\
          && script.crossOrigin === 'anonymous'\n\
          && script.async === true\n\
          && script.defer === true;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let script = dom.get_element_by_id("s").expect("missing #s");
    assert_eq!(
      dom.get_attribute(script, "src").unwrap(),
      Some("https://example.com/app.js")
    );
    assert_eq!(dom.get_attribute(script, "type").unwrap(), Some("module"));
    assert_eq!(dom.get_attribute(script, "charset").unwrap(), Some("utf-8"));
    assert_eq!(
      dom.get_attribute(script, "crossorigin").unwrap(),
      Some("anonymous")
    );
    assert_eq!(dom.has_attribute(script, "async").unwrap(), true);
    assert_eq!(dom.has_attribute(script, "defer").unwrap(), true);

    Ok(())
  }

  #[test]
  fn html_script_element_async_reflects_force_async_slot() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const s = document.createElement('script');\n\
        if (!(s.async === true && s.getAttribute('async') === null)) return false;\n\
        s.async = false;\n\
        if (!(s.async === false && s.getAttribute('async') === null)) return false;\n\
        s.async = true;\n\
        if (!(s.async === true && s.getAttribute('async') === '')) return false;\n\
        const t = document.createElement('script');\n\
        if (!(t.async === true && t.getAttribute('async') === null)) return false;\n\
        t.setAttribute('async', '');\n\
        t.removeAttribute('async');\n\
        return t.async === false && t.getAttribute('async') === null;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn parser_inserted_script_defaults_to_async_false() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><script id=s></script></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const s = document.getElementById('s');\n\
        return s.async === false && s.getAttribute('async') === null;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn element_style_shim_mutates_dom2_document() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const div = document.createElement('div');\n\
        div.id = 't';\n\
        div.style.display = 'none';\n\
        div.style.setProperty('cursor', 'pointer');\n\
        div.style.height = '10px';\n\
        div.style.setProperty('backgroundColor', 'red');\n\
        const removed = div.style.removeProperty('display');\n\
        document.body.appendChild(div);\n\
        return removed === 'none'\n\
          && div.style.getPropertyValue('background-color') === 'red'\n\
          && div.style.display === '';\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let div = dom.get_element_by_id("t").expect("missing #t");
    assert_eq!(
      dom.get_attribute(div, "style").unwrap(),
      Some("background-color: red; cursor: pointer; height: 10px;")
    );

    Ok(())
  }

  #[test]
  fn element_inner_html_round_trips_via_window_realm_shim() -> Result<(), VmError> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    realm.exec_script("document.getElementById('target').innerHTML = '<span>hi</span>'")?;
    let inner = realm.exec_script("document.getElementById('target').innerHTML")?;
    assert_eq!(get_string(realm.heap(), inner), "<span>hi</span>");

    let target = dom.get_element_by_id("target").expect("missing #target");
    assert_eq!(dom.inner_html(target).unwrap(), "<span>hi</span>");
    Ok(())
  }

  #[test]
  fn element_outer_html_setter_replaces_node_in_dom2_via_window_realm_shim() -> Result<(), VmError>
  {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=child>hi</span></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    realm.exec_script("document.getElementById('child').outerHTML = '<p>one</p><p>two</p>'")?;

    let root = dom.get_element_by_id("root").expect("missing #root");
    assert_eq!(dom.inner_html(root).unwrap(), "<p>one</p><p>two</p>");

    let outer = realm.exec_script("document.getElementById('root').outerHTML")?;
    assert_eq!(
      get_string(realm.heap(), outer),
      r#"<div id="root"><p>one</p><p>two</p></div>"#
    );

    Ok(())
  }

  #[test]
  fn element_insert_adjacent_html_inserts_fragment_and_returns_undefined() -> Result<(), VmError> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let result = realm.exec_script(
      "document.getElementById('target').insertAdjacentHTML('beforeend', '<b>hi</b>')",
    )?;
    assert_eq!(result, Value::Undefined);

    let target = dom.get_element_by_id("target").expect("missing #target");
    assert_eq!(dom.inner_html(target).unwrap(), "<b>hi</b>");

    let err_name = realm.exec_script(
      "try { document.getElementById('target').insertAdjacentHTML('nope', '<b>x</b>'); 'no' } catch (e) { e.name }",
    )?;
    assert_eq!(get_string(realm.heap(), err_name), "SyntaxError");

    Ok(())
  }

  #[test]
  fn element_insert_adjacent_text_inserts_text() -> Result<(), VmError> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    realm.exec_script("document.getElementById('target').insertAdjacentText('afterbegin', 'x')")?;

    let target = dom.get_element_by_id("target").expect("missing #target");
    assert_eq!(dom.inner_html(target).unwrap(), "x");
    Ok(())
  }

  #[test]
  fn element_insert_adjacent_element_inserts_and_returns_element() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=target></span></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let inserted_is_same = realm.exec_script(
      "(() => {\n\
        const b = document.createElement('b');\n\
        b.appendChild(document.createElement('i'));\n\
        const target = document.getElementById('target');\n\
        return target.insertAdjacentElement('beforebegin', b) === b;\n\
      })()",
    )?;
    assert_eq!(inserted_is_same, Value::Bool(true));

    let root = dom.get_element_by_id("root").expect("missing #root");
    assert_eq!(
      dom.inner_html(root).unwrap(),
      r#"<b><i></i></b><span id="target"></span>"#
    );

    Ok(())
  }

  #[test]
  fn document_create_text_node_and_node_text_content_round_trip() -> Result<(), VmError> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=root></div></body></html>")
        .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    // This test performs several DOM operations (including fragment parsing for `innerHTML`). Run
    // them across multiple script evaluations so each call stays within the per-run VM wall-time
    // budget.
    let step1 = realm.exec_script(
      "(() => {\n\
        const root = document.getElementById('root');\n\
        const t = document.createTextNode('hello');\n\
        root.appendChild(t);\n\
        if (t.textContent !== 'hello') return 't1:' + t.textContent;\n\
        if (root.textContent !== 'hello') return 'r1:' + root.textContent;\n\
\n\
        t.textContent = 'world';\n\
        if (root.textContent !== 'world') return 'r2:' + root.textContent;\n\
        if (root.innerHTML !== 'world') return 'html1:' + root.innerHTML;\n\
        return 'ok';\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), step1), "ok");

    let step2 = realm.exec_script(
      "(() => {\n\
        const root = document.getElementById('root');\n\
        root.innerHTML = '<span>hi</span><b>!</b>';\n\
        if (root.textContent !== 'hi!') return 'r3:' + root.textContent;\n\
        return 'ok';\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), step2), "ok");

    let step3 = realm.exec_script(
      "(() => {\n\
        const root = document.getElementById('root');\n\
        root.textContent = 'x';\n\
        if (root.innerHTML !== 'x') return 'html2:' + root.innerHTML;\n\
        root.textContent = '';\n\
        if (root.innerHTML !== '') return 'html3:' + root.innerHTML;\n\
        return 'ok';\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), step3), "ok");

    let step4 = realm.exec_script(
      "(() => {\n\
        const root = document.getElementById('root');\n\
        const frag = document.createDocumentFragment();\n\
        frag.appendChild(document.createTextNode('a'));\n\
        frag.appendChild(document.createTextNode('b'));\n\
        root.appendChild(frag);\n\
        if (root.textContent !== 'ab') return 'frag1:' + root.textContent;\n\
        if (frag.textContent !== '') return 'frag2:' + frag.textContent;\n\
        return 'ok';\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), step4), "ok");

    let step5 = realm.exec_script(
      "(() => {\n\
        const root = document.getElementById('root');\n\
        const c = document.createComment('ignore');\n\
        root.appendChild(c);\n\
        if (c.textContent !== 'ignore') return 'c1:' + c.textContent;\n\
        c.textContent = 'changed';\n\
        if (c.textContent !== 'changed') return 'c2:' + c.textContent;\n\
        if (root.textContent !== 'ab') return 'c3:' + root.textContent;\n\
        return 'ok';\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), step5), "ok");

    let step6 = realm.exec_script(
      "(() => {\n\
        const docNode = document.documentElement.parentNode;\n\
        if (docNode.textContent !== null) return 'doc:' + docNode.textContent;\n\
        return 'ok';\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), step6), "ok");

    let root = dom.get_element_by_id("root").expect("missing #root");
    assert_eq!(dom.inner_html(root).unwrap(), "ab");

    Ok(())
  }

  #[test]
  fn node_remove_child_detaches_and_returns_child() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=child>hi</span><b id=other></b></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const root = document.getElementById('root');\n\
        const child = document.getElementById('child');\n\
        const removed = root.removeChild(child);\n\
        let errName = 'no';\n\
        try { root.removeChild(document.createElement('p')); } catch (e) { errName = e.name; }\n\
        return removed === child && errName === 'NotFoundError';\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let root = dom.get_element_by_id("root").expect("missing #root");
    assert_eq!(dom.inner_html(root).unwrap(), r#"<b id="other"></b>"#);

    Ok(())
  }

  #[test]
  fn node_insert_before_inserts_before_reference_or_appends_on_null() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=ref>hi</span></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const root = document.getElementById('root');\n\
        const ref = document.getElementById('ref');\n\
        const before = document.createElement('b');\n\
        before.id = 'before';\n\
        const after = document.createElement('i');\n\
        after.id = 'after';\n\
        const ok1 = root.insertBefore(before, ref) === before;\n\
        const ok2 = root.insertBefore(after, null) === after;\n\
        return ok1 && ok2 && root.innerHTML === '<b id=\"before\"></b><span id=\"ref\">hi</span><i id=\"after\"></i>';\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let root = dom.get_element_by_id("root").expect("missing #root");
    assert_eq!(
      dom.inner_html(root).unwrap(),
      r#"<b id="before"></b><span id="ref">hi</span><i id="after"></i>"#
    );

    Ok(())
  }

  #[test]
  fn node_replace_child_replaces_and_returns_old_child() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=old>hi</span></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const root = document.getElementById('root');\n\
        const old = document.getElementById('old');\n\
        const next = document.createElement('p');\n\
        next.id = 'new';\n\
        const replaced = root.replaceChild(next, old);\n\
        return replaced === old && root.innerHTML === '<p id=\"new\"></p>';\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let root = dom.get_element_by_id("root").expect("missing #root");
    assert_eq!(dom.inner_html(root).unwrap(), r#"<p id="new"></p>"#);

    Ok(())
  }

  #[test]
  fn node_clone_node_clones_subtree_and_preserves_attributes() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=a><span>hello</span></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const div = document.getElementById('a');\n\
        const deep = div.cloneNode(true);\n\
        const shallow = div.cloneNode();\n\
        return deep !== div\n\
          && deep.outerHTML === '<div id=\"a\"><span>hello</span></div>'\n\
          && shallow.outerHTML === '<div id=\"a\"></div>';\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn node_traversal_accessors_follow_dom2_tree() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=a></span><span id=b></span></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const root = document.getElementById('root');\n\
        const a = document.getElementById('a');\n\
        const b = document.getElementById('b');\n\
        return a.parentNode === root\n\
          && a.parentElement === root\n\
          && root.firstChild === a\n\
          && root.lastChild === b\n\
          && a.previousSibling === null\n\
          && a.nextSibling === b\n\
          && b.previousSibling === a\n\
          && b.nextSibling === null;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn node_remove_detaches_from_dom2_document() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=a></span><span id=b></span></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const a = document.getElementById('a');\n\
        a.remove();\n\
        const root = document.getElementById('root');\n\
        return document.getElementById('a') === null\n\
          && a.parentNode === null\n\
          && root.innerHTML === '<span id=\"b\"></span>';\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let root = dom.get_element_by_id("root").expect("missing #root");
    assert_eq!(dom.inner_html(root).unwrap(), r#"<span id="b"></span>"#);

    Ok(())
  }

  #[test]
  fn node_child_nodes_is_live_and_cached_across_mutations() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        function clear_children(node) {\n\
          while (node.childNodes.length !== 0) {\n\
            node.removeChild(node.childNodes[0]);\n\
          }\n\
        }\n\
        const body = document.body;\n\
        clear_children(body);\n\
\n\
        const parent = document.createElement('div');\n\
        body.appendChild(parent);\n\
\n\
        const fragment = document.createDocumentFragment();\n\
        const a = document.createElement('span');\n\
        a.id = 'a';\n\
        const b = document.createElement('span');\n\
        b.id = 'b';\n\
        fragment.appendChild(a);\n\
        fragment.appendChild(b);\n\
\n\
        const parent_nodes = parent.childNodes;\n\
        const frag_nodes = fragment.childNodes;\n\
        if (!(parent_nodes.length === 0\n\
              && frag_nodes.length === 2\n\
              && frag_nodes[0] === a\n\
              && frag_nodes[1] === b)) return false;\n\
\n\
        const returned = parent.appendChild(fragment);\n\
        if (returned !== fragment) return false;\n\
\n\
        // Cached arrays should update in place.\n\
        if (!(parent_nodes.length === 2\n\
              && parent_nodes[0] === a\n\
              && parent_nodes[1] === b)) return false;\n\
        if (frag_nodes.length !== 0) return false;\n\
        if (fragment.parentNode !== null) return false;\n\
        if (a.parentNode !== parent || b.parentNode !== parent) return false;\n\
        if (parent.childNodes !== parent_nodes) return false;\n\
        if (fragment.childNodes !== frag_nodes) return false;\n\
\n\
        return true;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn get_element_by_id_skips_inert_template_and_fragments_are_searchable() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        function clear_children(node) {\n\
          while (node.childNodes.length !== 0) {\n\
            node.removeChild(node.childNodes[0]);\n\
          }\n\
        }\n\
        clear_children(document.body);\n\
\n\
        if (document.getElementById('missing') !== null) return false;\n\
\n\
        const tmpl = document.createElement('template');\n\
        tmpl.id = 'tmpl';\n\
        document.body.appendChild(tmpl);\n\
        const inside = document.createElement('div');\n\
        inside.id = 'inside';\n\
        tmpl.appendChild(inside);\n\
        if (document.getElementById('tmpl') !== tmpl) return false;\n\
        if (document.getElementById('inside') !== null) return false;\n\
\n\
        const frag = document.createDocumentFragment();\n\
        const f1 = document.createElement('div');\n\
        f1.id = 'f1';\n\
        frag.appendChild(f1);\n\
        const f2 = document.createElement('div');\n\
        f2.id = 'f2';\n\
        frag.appendChild(f2);\n\
        if (frag.getElementById('missing') !== null) return false;\n\
        if (frag.getElementById('f1') !== f1) return false;\n\
        if (frag.getElementById('f2') !== f2) return false;\n\
        if (document.getElementById('f1') !== null) return false;\n\
\n\
        return true;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn text_node_basics_instanceof_owner_document_and_is_connected() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id),
    )?;

    let ok = realm.exec_script(
      "(() => {\n\
        const text = document.createTextNode('hi');\n\
        if (!(text instanceof Text)) return false;\n\
        if (!(text instanceof Node)) return false;\n\
        if (text.nodeType !== Node.TEXT_NODE) return false;\n\
        if (text.nodeName !== '#text') return false;\n\
        if (text.data !== 'hi') return false;\n\
        text.data = 'a&b<>';\n\
        if (text.data !== 'a&b<>') return false;\n\
        text.textContent = 'x';\n\
        if (text.data !== 'x') return false;\n\
\n\
        const el = document.createElement('div');\n\
        if (el.isConnected) return false;\n\
        if (el.ownerDocument !== document) return false;\n\
        document.body.appendChild(el);\n\
        if (!el.isConnected) return false;\n\
        if (text.isConnected) return false;\n\
        el.appendChild(text);\n\
        if (!text.isConnected) return false;\n\
        if (text.ownerDocument !== document) return false;\n\
\n\
        const frag = document.createDocumentFragment();\n\
        if (frag.isConnected) return false;\n\
        if (frag.ownerDocument !== document) return false;\n\
        if (document.ownerDocument !== null) return false;\n\
        if (!document.isConnected) return false;\n\
\n\
        if (!(document instanceof Document)) return false;\n\
        if (!(el instanceof Element)) return false;\n\
        if (!(frag instanceof DocumentFragment)) return false;\n\
        return true;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn window_realm_shims_exist_and_are_linked() -> Result<(), VmError> {
    let url = "https://example.com/path";
    let mut realm = new_realm(WindowRealmConfig::new(url))?;

    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();

    let window = get_prop(vm, &mut scope, global, "window")?;
    let global_this = get_prop(vm, &mut scope, global, "globalThis")?;
    let self_ = get_prop(vm, &mut scope, global, "self")?;

    assert_eq!(window, global_this);
    assert_eq!(self_, window);
    assert_eq!(window, Value::Object(global));

    let location = get_prop(vm, &mut scope, global, "location")?;
    let Value::Object(location_obj) = location else {
      panic!("expected object");
    };
    let href_key_s = scope.alloc_string("href")?;
    scope.push_root(Value::String(href_key_s))?;
    let href_key = PropertyKey::from_string(href_key_s);
    let href = vm.get(&mut scope, location_obj, href_key)?;
    assert_eq!(get_string(scope.heap(), href), url);

    let origin_key_s = scope.alloc_string("origin")?;
    scope.push_root(Value::String(origin_key_s))?;
    let origin_key = PropertyKey::from_string(origin_key_s);
    let origin = vm.get(&mut scope, location_obj, origin_key)?;
    assert_eq!(get_string(scope.heap(), origin), "https://example.com");

    let document = get_prop(vm, &mut scope, global, "document")?;
    let Value::Object(document_obj) = document else {
      panic!("expected object");
    };
    let doc_url = get_prop(vm, &mut scope, document_obj, "URL")?;
    assert_eq!(get_string(scope.heap(), doc_url), url);

    let doc_location = get_prop(vm, &mut scope, document_obj, "location")?;
    assert_eq!(doc_location, Value::Object(location_obj));

    let console = get_prop(vm, &mut scope, global, "console")?;
    let Value::Object(console_obj) = console else {
      panic!("expected object");
    };
    let log = get_prop(vm, &mut scope, console_obj, "log")?;
    let Value::Object(log_func) = log else {
      panic!("expected console.log to be a function object");
    };

    // `console.log` is a host-created native function; ensure it inherits from `Function.prototype`
    // by calling it through `Function.prototype.call`.
    let mut host_hooks = NoopHostHooks::default();
    let call_key_s = scope.alloc_string("call")?;
    scope.push_root(Value::String(call_key_s))?;
    let call_key = PropertyKey::from_string(call_key_s);
    let call = vm.get(&mut scope, log_func, call_key)?;
    let call_result = vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      call,
      Value::Object(log_func),
      &[Value::Object(console_obj), Value::Number(1.0), Value::Null],
    )?;
    assert_eq!(call_result, Value::Undefined);

    Ok(())
  }

  #[test]
  fn window_or_worker_global_scope_primitives_exist_and_behave() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/path"))?;
    let window_origin = realm.exec_script("window.origin")?;
    assert_eq!(
      get_string(realm.heap(), window_origin),
      "https://example.com"
    );
    let origin = realm.exec_script("origin")?;
    assert_eq!(get_string(realm.heap(), origin), "https://example.com");
    assert_eq!(realm.exec_script("isSecureContext")?, Value::Bool(true));
    assert_eq!(
      realm.exec_script("crossOriginIsolated")?,
      Value::Bool(false)
    );

    let btoa_a = realm.exec_script("btoa('a')")?;
    assert_eq!(get_string(realm.heap(), btoa_a), "YQ==");
    let atob_a = realm.exec_script("atob('YQ==')")?;
    assert_eq!(get_string(realm.heap(), atob_a), "a");
    let atob_ws = realm.exec_script("atob(' Y Q = =\\n')")?;
    assert_eq!(get_string(realm.heap(), atob_ws), "a");
    let atob_no_pad = realm.exec_script("atob('YQ')")?;
    assert_eq!(get_string(realm.heap(), atob_no_pad), "a");

    let invalid_atob = realm.exec_script("try { atob('!!!'); 'no' } catch (e) { e.name }")?;
    assert_eq!(
      get_string(realm.heap(), invalid_atob),
      "InvalidCharacterError"
    );
    let invalid_btoa = realm.exec_script("try { btoa('\\u0100'); 'no' } catch (e) { e.name }")?;
    assert_eq!(
      get_string(realm.heap(), invalid_btoa),
      "InvalidCharacterError"
    );

    // `reportError` must never throw (even for Symbols).
    let report_ok =
      realm.exec_script("try { reportError(Symbol('x')); true } catch (e) { false }")?;
    assert_eq!(report_ok, Value::Bool(true));

    // atob result is a ByteString-like DOMString where each code unit is 0..255.
    let bytes = realm.exec_script("atob('AAECAw==')")?;
    let Value::String(handle) = bytes else {
      panic!("expected atob to return a string");
    };
    let units = realm.heap().get_string(handle)?.as_code_units().to_vec();
    assert_eq!(units, vec![0u16, 1, 2, 3]);

    Ok(())
  }

  #[test]
  fn window_realm_init_error_does_not_leak_console_sink() {
    let _lock = console_sink_test_lock()
      .lock()
      .expect("console sink test mutex should not be poisoned");
    let initial_len = console_sinks().lock().len();
    let sink: ConsoleSink = Arc::new(|_level, _heap, _args| {});

    let probe = |max_bytes: usize| -> (bool, bool) {
      let before_next = NEXT_CONSOLE_SINK_ID.load(Ordering::Relaxed);

      let mut js_options = JsExecutionOptions::default();
      js_options.max_vm_heap_bytes = Some(max_bytes);
      let mut config = WindowRealmConfig::new("https://example.com/")
        .with_js_execution_options(js_options);
      config.console_sink = Some(sink.clone());

      let res = WindowRealm::new(config);
      let registered = NEXT_CONSOLE_SINK_ID.load(Ordering::Relaxed) != before_next;
      let ok = res.is_ok();
      drop(res);

      assert_eq!(
        console_sinks().lock().len(),
        initial_len,
        "console sink map should be leak-free (max_bytes={max_bytes}, registered={registered}, ok={ok})"
      );
      (ok, registered)
    };

    // Find the minimal heap limit that allows initialization to succeed.
    let mut hi = 1024usize;
    loop {
      let (ok, _) = probe(hi);
      if ok {
        break;
      }
      hi = hi.saturating_mul(2);
      assert!(
        hi <= 64 * 1024 * 1024,
        "failed to find a heap limit that allows WindowRealm initialization"
      );
    }

    let mut lo = 0usize;
    let mut high = hi;
    while lo.saturating_add(1) < high {
      let mid = (lo + high) / 2;
      let (ok, _) = probe(mid);
      if ok {
        high = mid;
      } else {
        lo = mid;
      }
    }
    let succ_min = high;

    // Find the minimal heap limit that gets far enough to register a console sink.
    let mut lo = 0usize;
    let mut high = succ_min;
    while lo.saturating_add(1) < high {
      let mid = (lo + high) / 2;
      let (_, registered) = probe(mid);
      if registered {
        high = mid;
      } else {
        lo = mid;
      }
    }
    let reg_min = high;

    assert!(
      reg_min < succ_min,
      "expected some heap limits to register a console sink but still fail (reg_min={reg_min}, succ_min={succ_min})"
    );

    let (ok, registered) = probe(succ_min.saturating_sub(1));
    assert!(!ok, "expected init to fail just below succ_min");
    assert!(
      registered,
      "expected init to register a console sink before failing"
    );
  }

  #[test]
  fn console_sink_receives_log_arguments() -> Result<(), VmError> {
    let _lock = console_sink_test_lock()
      .lock()
      .expect("console sink test mutex should not be poisoned");
    let url = "https://example.com/path";
    let captured: Arc<Mutex<Vec<CapturedConsoleCall>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_sink = captured.clone();

    let sink: ConsoleSink = Arc::new(move |level, heap, args| {
      let args: Vec<CapturedConsoleArg> = args
        .iter()
        .map(|value| match *value {
          Value::Undefined => CapturedConsoleArg::Undefined,
          Value::Null => CapturedConsoleArg::Null,
          Value::Bool(b) => CapturedConsoleArg::Bool(b),
          Value::Number(n) => CapturedConsoleArg::Number(n),
          Value::BigInt(n) => CapturedConsoleArg::BigInt(n.to_decimal_string()),
          Value::String(s) => CapturedConsoleArg::String(
            heap
              .get_string(s)
              .expect("string handle should be valid")
              .to_utf8_lossy(),
          ),
          Value::Object(_) => CapturedConsoleArg::Object,
          Value::Symbol(_) => CapturedConsoleArg::Symbol,
        })
        .collect();
      captured_for_sink.lock().push(CapturedConsoleCall { level, args });
    });

    let mut config = WindowRealmConfig::new(url);
    config.console_sink = Some(sink);
    let mut realm = new_realm(config)?;

    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();

    let console = get_prop(vm, &mut scope, global, "console")?;
    let Value::Object(console_obj) = console else {
      panic!("expected object");
    };
    let mut host_hooks = NoopHostHooks::default();
    let calls = [
      ("log", ConsoleMessageLevel::Log, Value::Number(1.0)),
      ("info", ConsoleMessageLevel::Info, Value::Number(2.0)),
      ("warn", ConsoleMessageLevel::Warn, Value::Number(3.0)),
      ("error", ConsoleMessageLevel::Error, Value::Number(4.0)),
      ("debug", ConsoleMessageLevel::Debug, Value::Number(5.0)),
    ];
    for (name, _level, arg) in calls {
      let func = get_prop(vm, &mut scope, console_obj, name)?;
      let call_result = vm.call_with_host(
        &mut scope,
        &mut host_hooks,
        func,
        Value::Object(console_obj),
        &[arg],
      )?;
      assert_eq!(call_result, Value::Undefined);
    }

    assert_eq!(
      &*captured.lock(),
      &[
        CapturedConsoleCall {
          level: ConsoleMessageLevel::Log,
          args: vec![CapturedConsoleArg::Number(1.0)]
        },
        CapturedConsoleCall {
          level: ConsoleMessageLevel::Info,
          args: vec![CapturedConsoleArg::Number(2.0)]
        },
        CapturedConsoleCall {
          level: ConsoleMessageLevel::Warn,
          args: vec![CapturedConsoleArg::Number(3.0)]
        },
        CapturedConsoleCall {
          level: ConsoleMessageLevel::Error,
          args: vec![CapturedConsoleArg::Number(4.0)]
        },
        CapturedConsoleCall {
          level: ConsoleMessageLevel::Debug,
          args: vec![CapturedConsoleArg::Number(5.0)]
        },
      ]
    );
    Ok(())
  }

  #[test]
  fn atob_btoa_roundtrip() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;
    let encoded = realm.exec_script("btoa('hello')")?;
    assert_eq!(get_string(realm.heap(), encoded), "aGVsbG8=");

    let decoded = realm.exec_script("atob('aGVsbG8=')")?;
    assert_eq!(get_string(realm.heap(), decoded), "hello");
    Ok(())
  }

  #[test]
  fn atob_btoa_invalid_character_errors() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    {
      let err = realm
        .exec_script("btoa('☃')")
        .expect_err("btoa should throw InvalidCharacterError for non-Latin1 input");
      let obj = unwrap_thrown_object(err);
      let (_vm, heap) = realm.vm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(obj))?;
      let name = get_prop(_vm, &mut scope, obj, "name")?;
      assert_eq!(get_string(scope.heap(), name), "InvalidCharacterError");
    }

    {
      let err = realm
        .exec_script("atob('!')")
        .expect_err("atob should throw InvalidCharacterError for invalid base64");
      let obj = unwrap_thrown_object(err);
      let (_vm, heap) = realm.vm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(obj))?;
      let name = get_prop(_vm, &mut scope, obj, "name")?;
      assert_eq!(get_string(scope.heap(), name), "InvalidCharacterError");
    }

    Ok(())
  }

  #[test]
  fn external_interrupt_flag_interrupts_window_realm_and_is_not_reset() -> Result<(), VmError> {
    struct RestoreFlag {
      flag: Arc<AtomicBool>,
      prev: bool,
    }

    impl Drop for RestoreFlag {
      fn drop(&mut self) {
        self.flag.store(self.prev, Ordering::Relaxed);
      }
    }

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let flag = crate::render_control::interrupt_flag();
    let _guard = RestoreFlag {
      prev: flag.swap(true, Ordering::Relaxed),
      flag: Arc::clone(&flag),
    };

    let err = realm.exec_script("1 + 2").unwrap_err();
    match err {
      VmError::Termination(term) => assert_eq!(term.reason, vm_js::TerminationReason::Interrupted),
      other => panic!("expected interrupted termination, got {other:?}"),
    }

    realm.reset_interrupt();
    assert!(flag.load(Ordering::Relaxed));

    let err = realm.exec_script("1 + 2").unwrap_err();
    match err {
      VmError::Termination(term) => assert_eq!(term.reason, vm_js::TerminationReason::Interrupted),
      other => panic!("expected interrupted termination, got {other:?}"),
    }

    Ok(())
  }
}
