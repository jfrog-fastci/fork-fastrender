use crate::api::{BrowserDocumentDom2, BrowserTabHost, ConsoleMessageLevel};
use crate::dom2::{self, NodeId, NodeKind};
use crate::html::base_url_tracker::resolve_script_src_at_parse_time;
use crate::js::bindings::DomExceptionClassVmJs;
use crate::js::clock::{Clock, RealClock};
use crate::js::cookie_jar::{CookieJar, MAX_COOKIE_STRING_BYTES};
use crate::js::document_write::{current_document_write_state_mut, DocumentWriteLimitError};
use crate::js::dom_platform::{DomInterface, DomPlatform};
use crate::js::host_document::ActiveEventGuard;
use crate::js::realm_module_loader::{ModuleLoader, ModuleLoaderHandle};
use crate::js::time::{TimeBindings, WebTime};
use crate::js::window_env::{
  install_window_shims_vm_js, unregister_match_media_env, MatchMediaEnvGuard, WindowEnv,
};
use crate::js::window_timers::{
  event_loop_mut_from_hooks, hooks_have_event_loop, VmJsEventLoopHooks,
};
use crate::js::JsExecutionOptions;
use crate::js::{
  CurrentScriptStateHandle, DocumentHostState, EventLoop, ScriptOrchestrator, ScriptType,
  TaskSource, WindowHostState,
};
use crate::render_control;
use crate::resource::{
  cors_enforcement_enabled, ensure_cors_allows_origin, ensure_http_success,
  ensure_script_mime_sane, origin_from_url, CorsMode, FetchDestination, FetchRequest,
  ReferrerPolicy, ResourceFetcher,
};
use crate::style::media::MediaContext;
use crate::web::events as web_events;
use base64::engine::general_purpose;
use base64::Engine as _;
use parking_lot::Mutex;
use selectors::context::QuirksMode;
use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use url::Url;
use vm_js::{
  GcObject, GcString, Heap, HeapLimits, HostSlots, JsRuntime as VmJsRuntime, ModuleGraph,
  PropertyDescriptor, PropertyKey, PropertyKind, Realm, RealmId, Scope, SourceText, Value, Vm,
  VmError, VmHost, VmHostHooks, VmOptions,
};
use webidl_vm_js::VmJsHostHooksPayload;
use webidl_vm_js::WebIdlBindingsHost;

pub type ConsoleSink =
  Arc<dyn Fn(ConsoleMessageLevel, &mut vm_js::Heap, &[vm_js::Value]) + Send + Sync + 'static>;

/// Lightweight `VmHost` context used by `WindowRealm` helpers that need a stable downcast target.
///
/// `vm-js` passes a `&mut dyn VmHost` into every native call/construct handler. In full browser
/// embeddings this is typically a document/tab host type (e.g. [`DocumentHostState`]), but some
/// `WindowRealm` entrypoints create an internal host context instead.
///
/// We keep a dedicated type so helpers like `current_script_state_handle_from_vm_host` can reliably
/// recover shared host-side state (currently: `Document.currentScript` bookkeeping) even when the
/// embedder passes a unit `()` host or other opaque type.
#[derive(Clone, Debug, Default)]
struct VmJsHostContext {
  current_script_state: Option<CurrentScriptStateHandle>,
}

impl VmJsHostContext {
  fn current_script_state(&self) -> Option<&CurrentScriptStateHandle> {
    self.current_script_state.as_ref()
  }
}
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
  /// Optional override for the per-realm deterministic PRNG seed used by `window.crypto`.
  ///
  /// When unset, the seed defaults to a deterministic value derived from `document_url` so render
  /// output and tests remain reproducible across runs. Callers that prefer per-process randomness
  /// (e.g. a production browser embedding) can provide a random seed here.
  pub crypto_rng_seed: Option<u64>,
  /// Maximum size of a single Web Storage area (`localStorage` / `sessionStorage`), in UTF-16 bytes.
  pub web_storage_quota_utf16_bytes: usize,
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
      current_script_state: None,
      console_sink: None,
      heap_limits: super::vm_limits::default_heap_limits(),
      vm_options: VmOptions::default(),
      clock: Arc::new(RealClock::default()),
      web_time: WebTime::default(),
      crypto_rng_seed: None,
      web_storage_quota_utf16_bytes: 5 * 1024 * 1024,
    }
  }

  pub fn with_media_context(mut self, media: MediaContext) -> Self {
    self.media = media;
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

  pub fn with_crypto_rng_seed(mut self, seed: u64) -> Self {
    self.crypto_rng_seed = Some(seed);
    self
  }

  pub fn with_web_storage_quota_utf16_bytes(mut self, quota_bytes: usize) -> Self {
    self.web_storage_quota_utf16_bytes = quota_bytes;
    self
  }
}

pub struct WindowRealm {
  runtime: Box<VmJsRuntime>,
  realm_id: RealmId,
  console_sink_id: Option<u64>,
  match_media_env_id: Option<u64>,
  time_bindings: Option<TimeBindings>,
  interrupt_flag: Arc<AtomicBool>,
  /// Tracks whether `runtime.heap` is still alive.
  ///
  /// `vm-js` promise jobs can be queued onto the host event loop. When execution is aborted (due to
  /// budgets/cancellation), those queued jobs may be dropped without being run. Dropping a job
  /// without calling `Job::discard(..)` triggers debug assertions in `vm-js` because it would leak
  /// persistent roots.
  ///
  /// We use this flag to safely discard any abandoned jobs while the heap is still live, and fall
  /// back to leaking the job (only possible during teardown) once the heap is gone.
  heap_alive: Arc<AtomicBool>,
  js_execution_options: JsExecutionOptions,
  module_loader: ModuleLoaderHandle,
  next_script_id_raw: u64,
}

pub(crate) struct WindowRealmUserData {
  document_url: String,
  pub(crate) base_url: Option<String>,
  pending_navigation: Option<LocationNavigationRequest>,
  cookie_fetcher: Option<Arc<dyn ResourceFetcher>>,
  cookie_jar: CookieJar,
  pub(crate) module_loader: ModuleLoaderHandle,
  /// Optional module graph backing module scripts and dynamic `import()`.
  ///
  /// When present, `Vm::module_graph_ptr()` points into this boxed allocation.
  pub(crate) module_graph: Option<Box<ModuleGraph>>,
  dom_platform: Option<DomPlatform>,
  /// Deterministic per-realm PRNG state used by `window.crypto` RNG APIs.
  pub(crate) crypto_rng_state: u64,
  /// Fallback `dom2::Document` used for events when the realm is not backed by a host DOM.
  ///
  /// This enables `window`/`document` (and `new EventTarget()`) event listeners in minimal realms
  /// created without a host DOM.
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
      .field("has_module_graph", &self.module_graph.is_some())
      .field("has_dom_platform", &self.dom_platform.is_some())
      .field("crypto_rng_state", &self.crypto_rng_state)
      .field("has_window_obj", &self.window_obj.is_some())
      .field("has_document_obj", &self.document_obj.is_some())
      .finish()
  }
}

impl WindowRealmUserData {
  pub(crate) fn new(
    document_url: String,
    module_loader: ModuleLoaderHandle,
    crypto_rng_seed: Option<u64>,
  ) -> Self {
    let crypto_rng_state = crypto_rng_seed
      .map(crate::js::window_crypto::crypto_rng_seed_from_u64)
      .unwrap_or_else(|| crate::js::window_crypto::crypto_rng_seed_from_document_url(&document_url));
    Self {
      base_url: Some(document_url.clone()),
      pending_navigation: None,
      document_url,
      cookie_fetcher: None,
      cookie_jar: CookieJar::new(),
      module_loader,
      module_graph: None,
      dom_platform: None,
      crypto_rng_state,
      events_dom_fallback: dom2::Document::new(QuirksMode::NoQuirks),
      window_obj: None,
      document_obj: None,
    }
  }

  pub(crate) fn document_url(&self) -> &str {
    &self.document_url
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
    let heap_alive = Arc::new(AtomicBool::new(true));
    vm_options.interrupt_flag = Some(Arc::clone(&interrupt_flag));
    // Also observe the render-wide interrupt flag so host cancellation interrupts JS at the next
    // `Vm::tick()`.
    vm_options.external_interrupt_flag = Some(render_control::interrupt_flag());
    let vm = Vm::new(vm_options);
    let heap = Heap::new(heap_limits);

    let module_loader: ModuleLoaderHandle = Rc::new(RefCell::new({
      let mut loader = ModuleLoader::new(Some(config.document_url.clone()));
      loader.set_js_execution_options(js_execution_options);
      loader
    }));

    let mut runtime = Box::new(VmJsRuntime::new(vm, heap)?);
    runtime.vm.set_user_data(WindowRealmUserData::new(
      config.document_url.clone(),
      Rc::clone(&module_loader),
      config.crypto_rng_seed,
    ));
    let realm_id = runtime.realm().id();

    let (vm, realm, heap) = runtime.vm_realm_and_heap_mut();

    let (console_sink_id, match_media_env_id) = init_window_globals(vm, heap, realm, &config)?;
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
        if let Some(id) = match_media_env_id {
          unregister_match_media_env(id);
        }
        crate::js::window_url::teardown_window_url_bindings_for_realm(realm_id, heap);
        crate::js::window_blob::teardown_window_blob_bindings_for_realm(realm_id);
        crate::js::window_file::teardown_window_file_bindings_for_realm(realm_id);
        crate::js::window_form_data::teardown_window_form_data_bindings_for_realm(realm_id);
        return Err(err);
      }
    };
    Ok(Self {
      runtime,
      realm_id,
      console_sink_id,
      match_media_env_id,
      time_bindings: Some(time_bindings),
      interrupt_flag,
      heap_alive,
      js_execution_options,
      module_loader,
      next_script_id_raw: 0,
    })
  }

  pub fn new_with_js_execution_options(
    mut config: WindowRealmConfig,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self, VmError> {
    if js_execution_options.max_vm_heap_bytes.is_some() {
      // When explicitly configured, override any heap limits provided by the config. The chosen
      // limit is still clamped by the OS address-space ceiling (`RLIMIT_AS`) when tighter.
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
    realm
      .module_loader
      .borrow_mut()
      .set_js_execution_options(realm.js_execution_options);
    Ok(realm)
  }

  pub fn reset_interrupt(&self) {
    self.interrupt_flag.store(false, Ordering::Relaxed);
    self.runtime.vm.reset_interrupt();
  }

  pub(crate) fn vm_budget_now(&self) -> vm_js::Budget {
    self.js_execution_options.vm_js_budget_now()
  }

  pub fn js_execution_options(&self) -> JsExecutionOptions {
    self.js_execution_options
  }

  pub fn heap(&self) -> &Heap {
    &self.runtime.heap
  }

  pub fn heap_mut(&mut self) -> &mut Heap {
    &mut self.runtime.heap
  }

  pub(crate) fn heap_alive_flag(&self) -> &Arc<AtomicBool> {
    &self.heap_alive
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
    // If module support was enabled for this realm, `Vm::module_graph_ptr` points into the
    // realm-owned module graph allocation.
    //
    // `vm-js` module graphs store persistent GC roots (module environments/namespaces, cached
    // `import.meta`, and top-level await evaluation state). Dropping a graph without tearing it
    // down leaks those roots when the heap is reused.
    let module_graph = self
      .runtime
      .vm
      .user_data_mut::<WindowRealmUserData>()
      .and_then(|data| data.module_graph.take());
    if let Some(mut module_graph) = module_graph {
      module_graph.teardown(&mut self.runtime.vm, &mut self.runtime.heap);
    }
    self.runtime.vm.clear_module_graph();
    self.time_bindings.take();
    if let Some(id) = self.console_sink_id.take() {
      unregister_console_sink(id);
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
    crate::js::window_blob::teardown_window_blob_bindings_for_realm(realm_id);
    crate::js::window_file::teardown_window_file_bindings_for_realm(realm_id);
    crate::js::window_form_data::teardown_window_form_data_bindings_for_realm(realm_id);
    crate::js::window_url::teardown_window_url_bindings_for_realm(realm_id, &mut self.runtime.heap);
  }

  pub(crate) fn enable_module_loader(
    &mut self,
    fetcher: Arc<dyn ResourceFetcher>,
    document_origin: Option<crate::resource::DocumentOrigin>,
  ) -> Result<(), VmError> {
    // Configure the per-realm module loader to use the provided fetcher and limits. Callers can
    // still override per-request defaults (e.g. CORS mode) before executing scripts.
    {
      let mut loader = self.module_loader.borrow_mut();
      loader.set_fetcher(Arc::clone(&fetcher));
      loader.set_js_execution_options(self.js_execution_options);
      if document_origin.is_some() {
        loader.set_document_origin(document_origin);
      }
    }

    let vm = self.vm_mut();
    let graph_ptr: *mut ModuleGraph = {
      let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
        return Err(VmError::InvariantViolation(
          "window realm missing user data",
        ));
      };
      if data.module_graph.is_none() {
        data.module_graph = Some(Box::new(ModuleGraph::new()));
      }
      (&mut **data.module_graph.as_mut().expect("module_graph set above")) as *mut ModuleGraph
    };
    // SAFETY: the `ModuleGraph` is stored in a `Box` owned by the VM's `WindowRealmUserData` and is
    // cleared in `WindowRealm::teardown` before the user data is dropped.
    unsafe {
      vm.set_module_graph(&mut *graph_ptr);
    }
    Ok(())
  }

  pub fn set_cookie_fetcher(&mut self, fetcher: Arc<dyn ResourceFetcher>) {
    if let Some(data) = self.runtime.vm.user_data_mut::<WindowRealmUserData>() {
      data.cookie_fetcher = Some(fetcher);
    }
  }

  pub fn set_module_fetcher(&mut self, fetcher: Arc<dyn ResourceFetcher>) {
    self.module_loader.borrow_mut().set_fetcher(fetcher);
  }

  /// Update the document base URL used for resolving relative URLs in JS (e.g. `fetch("rel")` and
  /// `document.baseURI`).
  pub fn set_base_url(&mut self, base_url: Option<String>) {
    let effective_for_module_loader = if let Some(data) = self.runtime.vm.user_data_mut::<WindowRealmUserData>() {
      data.base_url = base_url;
      // When the embedder hasn't installed a base URL (or clears it), browsers still use the
      // document URL as the base URL. Keep module resolution consistent with `document.baseURI` and
      // `fetch("rel")` by falling back to `document_url` here as well.
      Some(
        data
          .base_url
          .clone()
          .unwrap_or_else(|| data.document_url.clone()),
      )
    } else {
      base_url
    };
    self
      .module_loader
      .borrow_mut()
      .set_document_url(effective_for_module_loader);
  }

  pub fn module_loader_handle(&self) -> ModuleLoaderHandle {
    Rc::clone(&self.module_loader)
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
    self.exec_script_with_name("<inline>", source)
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

    // If the realm does not have module loading enabled, skip the extra execution-context work:
    // dynamic `import()` will reject immediately and module loading hooks will never be invoked.
    if self.runtime.vm.module_graph_ptr().is_none() {
      return self
        .with_vm_budget(|rt| rt.exec_script_source_with_host_and_hooks(host, hooks, source));
    }

    let script_id = vm_js::ScriptId::from_raw(self.next_script_id_raw);
    self.next_script_id_raw = self.next_script_id_raw.wrapping_add(1);

    // Use the script's own URL as the referrer base for dynamic `import()` when the source name is
    // URL-like (external scripts). Otherwise fall back to the document base URL (inline scripts).
    let script_url = if Url::parse(source.name.as_ref()).is_ok() {
      source.name.to_string()
    } else {
      let Some(data) = self.runtime.vm.user_data_mut::<WindowRealmUserData>() else {
        return Err(VmError::InvariantViolation(
          "window realm missing user data",
        ));
      };
      data
        .base_url
        .clone()
        .unwrap_or_else(|| data.document_url.clone())
    };

    let module_loader = Rc::clone(&self.module_loader);
    let realm_id = self.realm_id;

    self.with_vm_budget(move |rt| {
      // We want `GetActiveScriptOrModule()` to return a Script Record while this script is running
      // so dynamic `import()` calls resolve relative to the script URL rather than the document.
      //
      // `vm-js`'s classic-script evaluator currently pushes an ExecutionContext whose
      // `script_or_module` is `None`, so we push our own Script-or-Module execution context around
      // the entire run. The engine will skip the inner `None` context and pick up this Script entry.
      struct ScriptReferrerGuard {
        vm: *mut Vm,
        module_loader: ModuleLoaderHandle,
        script_id: vm_js::ScriptId,
        exec_ctx: vm_js::ExecutionContext,
      }

      impl ScriptReferrerGuard {
        fn new(
          vm: &mut Vm,
          module_loader: ModuleLoaderHandle,
          realm: RealmId,
          script_id: vm_js::ScriptId,
          script_url: String,
        ) -> Result<Self, VmError> {
          {
            let mut loader = module_loader
              .try_borrow_mut()
              .map_err(|_| VmError::InvariantViolation("module loader already borrowed"))?;
            loader.register_script_url(script_id, script_url)?;
          }

          let exec_ctx = vm_js::ExecutionContext {
            realm,
            script_or_module: Some(vm_js::ScriptOrModule::Script(script_id)),
          };
          vm.push_execution_context(exec_ctx);

          Ok(Self {
            vm: vm as *mut Vm,
            module_loader,
            script_id,
            exec_ctx,
          })
        }
      }

      impl Drop for ScriptReferrerGuard {
        fn drop(&mut self) {
          // SAFETY: the guard is only constructed from a live `&mut Vm` owned by the current realm.
          // It is dropped before the realm (and its VM) can be dropped.
          let vm = unsafe { &mut *self.vm };
          let popped = vm.pop_execution_context();
          debug_assert_eq!(popped, Some(self.exec_ctx));

          // Best-effort: do not panic if the module loader is already borrowed (should not happen).
          if let Ok(mut loader) = self.module_loader.try_borrow_mut() {
            loader.unregister_script_url(self.script_id);
          }
        }
      }

      let _guard =
        ScriptReferrerGuard::new(&mut rt.vm, module_loader, realm_id, script_id, script_url)?;

      rt.exec_script_source_with_host_and_hooks(host, hooks, source)
    })
  }

  pub(crate) fn exec_script_source_with_hooks(
    &mut self,
    hooks: &mut dyn VmHostHooks,
    source: Arc<SourceText>,
  ) -> Result<Value, VmError> {
    let mut host_ctx = DocumentHostState::new(dom2::Document::new(QuirksMode::NoQuirks));
    self.exec_script_source_with_host_and_hooks(&mut host_ctx, hooks, source)
  }

  /// Execute a classic script with an explicit source name for stack traces.
  pub fn exec_script_with_name(
    &mut self,
    source_name: impl Into<Arc<str>>,
    source_text: impl Into<Arc<str>>,
  ) -> Result<Value, VmError> {
    // The `vm-js` default `exec_script` path uses the VM-owned microtask queue as the active
    // `VmHostHooks` implementation. That queue does not provide FastRender's DOM shims (like
    // `Element.dataset`), which rely on `VmHostHooks::{host_exotic_get,host_exotic_set,host_exotic_delete}`.
    //
    // Install a lightweight host hook wrapper for the duration of the run, while still preserving
    // the VM-owned microtask queue for Promise jobs.

    struct MicrotaskQueueRestoreGuard {
      vm: *mut Vm,
      queue: vm_js::MicrotaskQueue,
    }

    impl MicrotaskQueueRestoreGuard {
      fn new(vm: &mut Vm) -> Self {
        let queue = std::mem::take(vm.microtask_queue_mut());
        Self {
          vm: vm as *mut Vm,
          queue,
        }
      }
    }

    impl Drop for MicrotaskQueueRestoreGuard {
      fn drop(&mut self) {
        // SAFETY: the guard is only constructed from a live `&mut Vm` owned by the current realm.
        // It is dropped before the realm (and its VM) can be dropped.
        let vm = unsafe { &mut *self.vm };
        *vm.microtask_queue_mut() = std::mem::take(&mut self.queue);
      }
    }

    struct WindowRealmDomShimHooks<'a> {
      microtasks: &'a mut vm_js::MicrotaskQueue,
      any: VmJsHostHooksPayload,
    }

    impl vm_js::VmHostHooks for WindowRealmDomShimHooks<'_> {
      fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<vm_js::RealmId>) {
        self.microtasks.enqueue_promise_job(job, realm);
      }

      fn host_exotic_get(
        &mut self,
        scope: &mut Scope<'_>,
        obj: GcObject,
        key: PropertyKey,
        receiver: Value,
      ) -> Result<Option<Value>, VmError> {
        let _ = receiver;
        dataset_exotic_get(scope, self.any.vm_host_mut(), obj, key)
      }

      fn host_exotic_set(
        &mut self,
        scope: &mut Scope<'_>,
        obj: GcObject,
        key: PropertyKey,
        value: Value,
        receiver: Value,
      ) -> Result<Option<bool>, VmError> {
        let _ = receiver;
        dataset_exotic_set(scope, self.any.vm_host_mut(), obj, key, value)
      }

      fn host_exotic_delete(
        &mut self,
        scope: &mut Scope<'_>,
        obj: GcObject,
        key: PropertyKey,
      ) -> Result<Option<bool>, VmError> {
        dataset_exotic_delete(scope, self.any.vm_host_mut(), obj, key)
      }

      fn host_call_job_callback(
        &mut self,
        ctx: &mut dyn vm_js::VmJobContext,
        callback: &vm_js::JobCallback,
        this_argument: Value,
        arguments: &[Value],
      ) -> Result<Value, VmError> {
        // Mirror `vm-js`'s built-in `MicrotaskQueue` implementation, but pass `self` so queued jobs
        // see the same DOM shim host hooks as the originating script.
        ctx.call(
          self,
          Value::Object(callback.callback_object()),
          this_argument,
          arguments,
        )
      }

      fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(&mut self.any)
      }
    }

    let source = Arc::new(SourceText::new(source_name, source_text));

    self.with_vm_budget(move |rt| {
      // Temporarily move the VM-owned microtask queue out so we can both:
      // - expose DOM shim exotic hooks to the evaluator, and
      // - keep Promise jobs enqueued onto the VM-owned queue (restored on drop).
      let mut guard = MicrotaskQueueRestoreGuard::new(&mut rt.vm);
      let mut host_ctx = DocumentHostState::new(dom2::Document::new(QuirksMode::NoQuirks));
      let mut any = VmJsHostHooksPayload::default();
      any.set_vm_host(&mut host_ctx);
      let mut hooks = WindowRealmDomShimHooks {
        microtasks: &mut guard.queue,
        any,
      };
      rt.exec_script_source_with_host_and_hooks(&mut host_ctx, &mut hooks, source)
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
    self.with_vm_budget(|rt| {
      // `vm-js`'s built-in `Vm::perform_microtask_checkpoint` runs queued jobs using a lightweight
      // internal `VmHostHooks` implementation that only supports Promise job chaining. FastRender's
      // DOM shims (notably `Element.dataset`) rely on `VmHostHooks::{host_exotic_get,host_exotic_set,host_exotic_delete}`.
      //
      // When embedders use `WindowRealm` directly (without the higher-level `EventLoop`/`WindowHost`
      // pipeline), we still want Promise callbacks to behave like script execution and see the same
      // DOM shim host hooks.

      if !rt.vm.microtask_queue_mut().begin_checkpoint() {
        return Ok(());
      }

      struct DomShimMicrotaskHooks {
        any: VmJsHostHooksPayload,
        pending: Vec<(Option<RealmId>, vm_js::Job)>,
      }

      impl DomShimMicrotaskHooks {
        fn new(host_ctx: &mut dyn VmHost) -> Self {
          let mut any = VmJsHostHooksPayload::default();
          any.set_vm_host(host_ctx);
          Self {
            any,
            pending: Vec::new(),
          }
        }

        fn drain_into(&mut self, queue: &mut vm_js::MicrotaskQueue) {
          for (realm, job) in self.pending.drain(..) {
            queue.enqueue_promise_job(job, realm);
          }
        }
      }

      impl VmHostHooks for DomShimMicrotaskHooks {
        fn host_enqueue_promise_job(&mut self, job: vm_js::Job, realm: Option<RealmId>) {
          self.pending.push((realm, job));
        }

        fn host_exotic_get(
          &mut self,
          scope: &mut Scope<'_>,
          obj: GcObject,
          key: PropertyKey,
          receiver: Value,
        ) -> Result<Option<Value>, VmError> {
          let _ = receiver;
          dataset_exotic_get(scope, self.any.vm_host_mut(), obj, key)
        }

        fn host_exotic_set(
          &mut self,
          scope: &mut Scope<'_>,
          obj: GcObject,
          key: PropertyKey,
          value: Value,
          receiver: Value,
        ) -> Result<Option<bool>, VmError> {
          let _ = receiver;
          dataset_exotic_set(scope, self.any.vm_host_mut(), obj, key, value)
        }

        fn host_exotic_delete(
          &mut self,
          scope: &mut Scope<'_>,
          obj: GcObject,
          key: PropertyKey,
        ) -> Result<Option<bool>, VmError> {
          dataset_exotic_delete(scope, self.any.vm_host_mut(), obj, key)
        }

        fn host_call_job_callback(
          &mut self,
          ctx: &mut dyn vm_js::VmJobContext,
          callback: &vm_js::JobCallback,
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

        fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
          Some(&mut self.any)
        }
      }

      struct Ctx<'a> {
        vm: &'a mut Vm,
        heap: &'a mut Heap,
        host: &'a mut dyn VmHost,
        realm: Option<RealmId>,
      }

      impl vm_js::VmJobContext for Ctx<'_> {
        fn call(
          &mut self,
          hooks: &mut dyn VmHostHooks,
          callee: Value,
          this: Value,
          args: &[Value],
        ) -> Result<Value, VmError> {
          let mut scope = self.heap.scope();
          if let Some(realm) = self.realm {
            let mut vm = self.vm.execution_context_guard(vm_js::ExecutionContext {
              realm,
              script_or_module: None,
            });
            vm.call_with_host_and_hooks(&mut *self.host, &mut scope, hooks, callee, this, args)
          } else {
            self
              .vm
              .call_with_host_and_hooks(&mut *self.host, &mut scope, hooks, callee, this, args)
          }
        }

        fn construct(
          &mut self,
          hooks: &mut dyn VmHostHooks,
          callee: Value,
          args: &[Value],
          new_target: Value,
        ) -> Result<Value, VmError> {
          let mut scope = self.heap.scope();
          if let Some(realm) = self.realm {
            let mut vm = self.vm.execution_context_guard(vm_js::ExecutionContext {
              realm,
              script_or_module: None,
            });
            vm.construct_with_host_and_hooks(
              &mut *self.host,
              &mut scope,
              hooks,
              callee,
              args,
              new_target,
            )
          } else {
            self.vm.construct_with_host_and_hooks(
              &mut *self.host,
              &mut scope,
              hooks,
              callee,
              args,
              new_target,
            )
          }
        }

        fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
          self.heap.add_root(value)
        }

        fn remove_root(&mut self, id: vm_js::RootId) {
          self.heap.remove_root(id);
        }
      }

      struct TeardownCtx<'a> {
        heap: &'a mut Heap,
      }

      impl vm_js::VmJobContext for TeardownCtx<'_> {
        fn call(
          &mut self,
          _hooks: &mut dyn VmHostHooks,
          _callee: Value,
          _this: Value,
          _args: &[Value],
        ) -> Result<Value, VmError> {
          Err(VmError::Unimplemented("TeardownCtx::call"))
        }

        fn construct(
          &mut self,
          _hooks: &mut dyn VmHostHooks,
          _callee: Value,
          _args: &[Value],
          _new_target: Value,
        ) -> Result<Value, VmError> {
          Err(VmError::Unimplemented("TeardownCtx::construct"))
        }

        fn add_root(&mut self, value: Value) -> Result<vm_js::RootId, VmError> {
          self.heap.add_root(value)
        }

        fn remove_root(&mut self, id: vm_js::RootId) {
          self.heap.remove_root(id);
        }
      }

      // Use a lightweight `VmHost` context for Promise job execution when the higher-level
      // `WindowHost`/event-loop pipeline is not in use.
      //
      // `HostDocumentState` carries the `Document.currentScript` handle, so Promise jobs run in this
      // fallback path can still observe/override `currentScript` when needed.
      let mut host_ctx = DocumentHostState::new(dom2::Document::new(QuirksMode::NoQuirks));
      let mut hooks = DomShimMicrotaskHooks::new(&mut host_ctx);

      let mut first_err: Option<VmError> = None;
      let mut termination_err: Option<VmError> = None;

      loop {
        let Some((realm, job)) = rt.vm.microtask_queue_mut().pop_front() else {
          break;
        };

        let job_result = {
          let mut ctx = Ctx {
            vm: &mut rt.vm,
            heap: &mut rt.heap,
            host: &mut host_ctx,
            realm,
          };
          job.run(&mut ctx, &mut hooks)
        };

        // Some job types may schedule new Promise jobs via `VmHostHooks`; enqueue them into the VM's
        // microtask queue before proceeding (or before discarding the remaining queue on
        // termination).
        hooks.drain_into(rt.vm.microtask_queue_mut());

        match job_result {
          Ok(()) => {}
          Err(e @ VmError::Termination(_)) => {
            termination_err = Some(e);
            break;
          }
          Err(e) => {
            if first_err.is_none() {
              first_err = Some(e);
            }
          }
        }
      }

      if let Some(err) = termination_err {
        // Termination is a hard stop: discard any remaining queued jobs (and any jobs enqueued by
        // the failing job) so we don't leak persistent roots.
        let mut ctx = TeardownCtx { heap: &mut rt.heap };
        rt.vm.microtask_queue_mut().teardown(&mut ctx);
        rt.vm.microtask_queue_mut().end_checkpoint();
        return Err(err);
      }

      rt.vm.microtask_queue_mut().end_checkpoint();
      match first_err {
        Some(err) => Err(err),
        None => Ok(()),
      }
    })
  }
}

pub trait WindowRealmHost {
  /// Borrow-splits the host into:
  /// - a mutable `VmHost` context for native calls, and
  /// - a mutable `WindowRealm` for script/job execution.
  ///
  /// Implementations must ensure these borrows do not alias.
  fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm);

  fn webidl_bindings_host(&mut self) -> Option<&mut dyn WebIdlBindingsHost> {
    None
  }

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
    self.heap_alive.store(false, Ordering::Relaxed);
    self.teardown();
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

fn read_only_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn non_configurable_read_only_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: false,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
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

#[cfg(test)]
pub(crate) fn test_only_create_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  ctor: GcObject,
  message: &str,
) -> Result<Value, VmError> {
  create_error(vm, scope, host, hooks, ctor, message)
}

fn throw_type_error(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  message: &str,
) -> VmError {
  let intr = match vm.intrinsics() {
    Some(intr) => intr,
    None => return VmError::TypeError("TypeError requires intrinsics (create a Realm first)"),
  };
  match create_error(vm, scope, host, hooks, intr.type_error(), message) {
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
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(throw_type_error(
    vm,
    scope,
    host,
    hooks,
    "Illegal constructor",
  ))
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
) -> Result<(GcObject, GcObject, usize), VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let this_slot = slots
    .get(STORAGE_METHOD_THIS_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);
  let data_slot = slots
    .get(STORAGE_METHOD_DATA_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);
  let quota_slot = slots
    .get(STORAGE_METHOD_QUOTA_UTF16_BYTES_SLOT)
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
  let Value::Number(quota_n) = quota_slot else {
    return Err(VmError::InvariantViolation(
      "Storage native missing quota slot",
    ));
  };
  if !quota_n.is_finite() || quota_n < 0.0 {
    return Err(VmError::InvariantViolation(
      "Storage native had invalid quota slot value",
    ));
  }
  Ok((expected_this, data_obj, quota_n as usize))
}

fn storage_require_this(
  scope: &Scope<'_>,
  callee: GcObject,
  this: Value,
) -> Result<(GcObject, usize), VmError> {
  let (expected_this, data_obj, quota_utf16_bytes) = storage_slots_from_callee(scope, callee)?;
  match this {
    Value::Object(obj) if obj == expected_this => Ok((data_obj, quota_utf16_bytes)),
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

fn storage_string_utf16_bytes(scope: &Scope<'_>, s: GcString) -> Result<usize, VmError> {
  Ok(
    scope
      .heap()
      .get_string(s)?
      .as_code_units()
      .len()
      .saturating_mul(2),
  )
}

fn storage_total_utf16_bytes(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  data_obj: GcObject,
) -> Result<usize, VmError> {
  let keys = scope.ordinary_own_property_keys_with_tick(data_obj, || vm.tick())?;

  let mut total: usize = 0;
  for (i, key) in keys.iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let PropertyKey::String(key_s) = key else {
      continue;
    };
    total = total.saturating_add(storage_string_utf16_bytes(scope, *key_s)?);
    match scope.heap().object_get_own_data_property_value(data_obj, key)? {
      Some(Value::String(value_s)) => {
        total = total.saturating_add(storage_string_utf16_bytes(scope, value_s)?)
      }
      _ => {}
    }
  }

  Ok(total)
}

fn storage_length_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (data_obj, _) = storage_require_this(scope, callee, this)?;
  let keys = scope.ordinary_own_property_keys_with_tick(data_obj, || vm.tick())?;

  let mut count: usize = 0;
  for (i, key) in keys.iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    if matches!(key, PropertyKey::String(_)) {
      count = count.saturating_add(1);
    }
  }

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
  let (data_obj, _) = storage_require_this(scope, callee, this)?;
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
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (data_obj, quota_utf16_bytes) = storage_require_this(scope, callee, this)?;
  let key_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let value_v = args.get(1).copied().unwrap_or(Value::Undefined);
  let key_s = storage_to_string(scope, key_v)?;
  let value_s = storage_to_string(scope, value_v)?;
  let key = PropertyKey::from_string(key_s);
  let value = Value::String(value_s);

  let current_size = storage_total_utf16_bytes(vm, scope, data_obj)?;
  let key_size = storage_string_utf16_bytes(scope, key_s)?;
  let value_size = storage_string_utf16_bytes(scope, value_s)?;
  let old_value = scope.heap().object_get_own_data_property_value(data_obj, &key)?;
  let old_value_size = match old_value {
    Some(Value::String(old_value_s)) => storage_string_utf16_bytes(scope, old_value_s)?,
    _ => 0,
  };
  let old_entry_size = if old_value.is_some() {
    key_size.saturating_add(old_value_size)
  } else {
    0
  };
  let new_entry_size = key_size.saturating_add(value_size);
  let new_size = current_size
    .saturating_add(new_entry_size)
    .saturating_sub(old_entry_size);
  if new_size > quota_utf16_bytes {
    return Err(VmError::Throw(make_dom_exception(
      scope,
      "QuotaExceededError",
      "The quota has been exceeded.",
    )?));
  }

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
  let (data_obj, _) = storage_require_this(scope, callee, this)?;
  let key_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let key_s = storage_to_string(scope, key_v)?;
  let key = PropertyKey::from_string(key_s);
  let _ = scope.ordinary_delete(data_obj, key)?;
  Ok(Value::Undefined)
}

fn storage_clear_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (data_obj, _) = storage_require_this(scope, callee, this)?;
  let keys = scope.ordinary_own_property_keys_with_tick(data_obj, || vm.tick())?;
  for (i, key) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let _ = scope.ordinary_delete(data_obj, key)?;
  }
  Ok(Value::Undefined)
}

fn storage_key_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (data_obj, _) = storage_require_this(scope, callee, this)?;
  let idx_v = args.get(0).copied().unwrap_or(Value::Undefined);
  let Some(idx) = storage_to_index(scope, idx_v)? else {
    return Ok(Value::Null);
  };
  let keys = scope.ordinary_own_property_keys_with_tick(data_obj, || vm.tick())?;

  let mut string_idx: usize = 0;
  for (i, key) in keys.into_iter().enumerate() {
    if i % 1024 == 0 {
      vm.tick()?;
    }
    let PropertyKey::String(s) = key else {
      continue;
    };
    if string_idx == idx {
      return Ok(Value::String(s));
    }
    string_idx = string_idx.saturating_add(1);
  }

  Ok(Value::Null)
}

fn install_storage_object(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  realm: &Realm,
  global: GcObject,
  global_key: PropertyKey,
  label: &str,
  quota_utf16_bytes: usize,
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

  let slots = [
    Value::Object(storage_obj),
    Value::Object(data_obj),
    Value::Number(quota_utf16_bytes as f64),
  ];

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
const HISTORY_OBJ_SLOT: usize = 0;
const HISTORY_LOCATION_OBJ_SLOT: usize = 1;
const HISTORY_DOCUMENT_OBJ_SLOT: usize = 2;
const HISTORY_REPLACE_SLOT: usize = 3;
const STORAGE_METHOD_THIS_SLOT: usize = 0;
const STORAGE_METHOD_DATA_SLOT: usize = 1;
const STORAGE_METHOD_QUOTA_UTF16_BYTES_SLOT: usize = 2;
const STORAGE_ILLEGAL_INVOCATION_ERROR: &str = "Illegal invocation";
const EVENT_TARGET_DEFAULT_THIS_SLOT: usize = 0;
const EVENT_TARGET_CONTEXT_GLOBAL_SLOT: usize = 1;
const EVENT_TARGET_CONTEXT_ABORT_CLEANUP_CALL_ID_SLOT: usize = 2;
const EVENT_TARGET_BRAND_KEY: &str = "__fastrender_event_target";
const EVENT_TARGET_PARENT_KEY: &str = "__fastrender_event_target_parent";
const ABORT_SIGNAL_BRAND_KEY: &str = "__fastrender_abort_signal";
const NODE_ID_KEY: &str = "__fastrender_node_id";
const DOM_STRING_MAP_HOST_KIND: u64 = 4;
const WRAPPER_DOCUMENT_KEY: &str = "__fastrender_wrapper_document";
const DOCUMENT_WINDOW_KEY: &str = "__fastrender_document_window";
const EVENT_PROTOTYPE_KEY: &str = "__fastrender_event_prototype";
const CUSTOM_EVENT_PROTOTYPE_KEY: &str = "__fastrender_custom_event_prototype";
const STORAGE_EVENT_PROTOTYPE_KEY: &str = "__fastrender_storage_event_prototype";
const EVENT_ID_KEY: &str = "__fastrender_event_id";
const EVENT_IMMEDIATE_STOP_KEY: &str = "__fastrender_event_stop_immediate";
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

// Must match `window_timers::INTERNAL_QUEUE_MICROTASK_KEY`, but duplicated here to avoid a module
// dependency cycle (`window_timers` depends on `window_realm`).
const INTERNAL_QUEUE_MICROTASK_KEY: &str = "__fastrender_queue_microtask";

const MUTATION_OBSERVER_ID_KEY: &str = "__fastrender_mutation_observer_id";
const MUTATION_OBSERVER_CALLBACK_KEY: &str = "__fastrender_mutation_observer_callback";
const MUTATION_OBSERVER_DOCUMENT_KEY: &str = "__fastrender_mutation_observer_document";
const MUTATION_OBSERVER_REGISTRY_KEY: &str = "__fastrender_mutation_observer_registry";
const MUTATION_OBSERVER_NOTIFY_KEY: &str = "__fastrender_mutation_observer_notify";

const MUTATION_OBSERVER_NOTIFY_DOCUMENT_SLOT: usize = 0;

static NEXT_CURRENT_SCRIPT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_ACTIVE_EVENT_ID: AtomicU64 = AtomicU64::new(1);

static NEXT_MUTATION_OBSERVER_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
  static CURRENT_SCRIPT_SOURCES: RefCell<HashMap<u64, CurrentScriptStateHandle>> =
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

fn event_active_event_id(
  scope: &mut Scope<'_>,
  event_obj: GcObject,
) -> Result<Option<u64>, VmError> {
  let key = alloc_key(scope, EVENT_ID_KEY)?;
  Ok(
    match scope
      .heap()
      .object_get_own_data_property_value(event_obj, &key)?
    {
      Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => Some(n as u64),
      _ => None,
    },
  )
}

fn push_active_event_for_host(
  host: &mut dyn VmHost,
  event_id: u64,
  event: &mut web_events::Event,
) -> Option<ActiveEventGuard> {
  let any = host.as_any_mut();
  if let Some(host) = any.downcast_mut::<DocumentHostState>() {
    return Some(host.push_active_event(event_id, event));
  }
  if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
    return Some(host.push_active_event(event_id, event));
  }
  None
}

fn with_active_event_for_host<R>(
  host: &mut dyn VmHost,
  event_id: u64,
  f: impl FnOnce(&mut web_events::Event) -> R,
) -> Option<R> {
  let any = host.as_any_mut();
  if let Some(host) = any.downcast_mut::<DocumentHostState>() {
    return host.with_active_event(event_id, f);
  }
  if let Some(host) = any.downcast_mut::<BrowserDocumentDom2>() {
    return host.with_active_event(event_id, f);
  }
  None
}

pub(crate) fn dataset_exotic_get(
  scope: &mut Scope<'_>,
  mut host: Option<&mut dyn VmHost>,
  obj: GcObject,
  key: PropertyKey,
) -> Result<Option<Value>, VmError> {
  // `host_exotic_get` is called for *all* objects, including VM-internal kinds like Promises and
  // typed arrays. `Heap::object_host_slots` only supports ordinary objects/functions; for other
  // object kinds, treat this as "no host slots" rather than failing the property access.
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
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

  let Some(host) = host.as_deref_mut() else {
    return Ok(None);
  };
  let Some(host) = crate::js::dom_host::dom_host_vmjs(host) else {
    return Ok(None);
  };

  let node_index = match usize::try_from(slots.a) {
    Ok(v) => v,
    Err(_) => return Ok(None),
  };
  let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();

  let node_id = match host.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(None),
  };
  let Some(value) = host.dataset_get(node_id, &prop) else {
    return Ok(None);
  };
  Ok(Some(Value::String(scope.alloc_string(&value)?)))
}

pub(crate) fn dataset_exotic_set(
  scope: &mut Scope<'_>,
  mut host: Option<&mut dyn VmHost>,
  obj: GcObject,
  key: PropertyKey,
  value: Value,
) -> Result<Option<bool>, VmError> {
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
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

  let Some(host) = host.as_deref_mut() else {
    return Ok(None);
  };
  let Some(host) = crate::js::dom_host::dom_host_vmjs(host) else {
    return Ok(None);
  };
  let node_index = match usize::try_from(slots.a) {
    Ok(v) => v,
    Err(_) => return Ok(None),
  };

  let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();
  let value_value = scope.heap_mut().to_string(value)?;
  let value = scope
    .heap()
    .get_string(value_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let node_id = match host.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(None),
  };
  if let Err(err) = host.dataset_set(node_id, &prop, &value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Some(true))
}

pub(crate) fn dataset_exotic_delete(
  scope: &mut Scope<'_>,
  mut host: Option<&mut dyn VmHost>,
  obj: GcObject,
  key: PropertyKey,
) -> Result<Option<bool>, VmError> {
  let slots = match scope.heap().object_host_slots(obj) {
    Ok(slots) => slots,
    Err(VmError::InvalidHandle { .. }) if scope.heap().is_valid_object(obj) => None,
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

  let Some(host) = host.as_deref_mut() else {
    return Ok(None);
  };
  let Some(host) = crate::js::dom_host::dom_host_vmjs(host) else {
    return Ok(None);
  };
  let node_index = match usize::try_from(slots.a) {
    Ok(v) => v,
    Err(_) => return Ok(None),
  };

  let prop = scope.heap().get_string(prop_s)?.to_utf8_lossy();

  let node_id = match host.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(None),
  };
  if let Err(err) = host.dataset_delete(node_id, &prop) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(Some(true))
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
  let level = match slots
    .get(CONSOLE_LEVEL_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
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
    _ => match slots
      .get(CONSOLE_THIS_SLOT)
      .copied()
      .unwrap_or(Value::Undefined)
    {
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

fn console_noop_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
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

fn location_to_string_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // `Location#toString` is defined to return the same value as `location.href`.
  //
  // Real browsers use a real Location prototype; FastRender models `location` as a plain object,
  // so keep the canonical URL in a private data property (`__fastrender_location_url`).
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

fn location_to_primitive_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  // `Location[@@toPrimitive]` should stringify to the current `href`.
  //
  // Ignore the hint argument: returning the URL string for all hints keeps common operations like
  // `location + ''` and `String(location)` aligned with browser expectations.
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
  host: &mut dyn VmHost,
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
    // Match `document.baseURI`: fall back to the document URL when no explicit base URL is set.
    data
      .base_url
      .clone()
      .unwrap_or_else(|| data.document_url.clone())
  };

  let url_value = match url_value {
    Value::String(s) => s,
    other => scope.heap_mut().to_string(other)?,
  };
  scope.push_root(Value::String(url_value))?;
  let url_input = scope.heap().get_string(url_value)?.to_utf8_lossy();

  let resolved = crate::js::url_resolve::resolve_url(&url_input, Some(base_url.as_str()))
    .map_err(|err| throw_type_error(vm, scope, host, hooks, &err.to_string()))?;

  let parsed = Url::parse(&resolved)
    .map_err(|err| throw_type_error(vm, scope, host, hooks, &err.to_string()))?;
  match parsed.scheme() {
    "http" | "https" | "file" | "data" | "about" => {}
    other => {
      return Err(throw_type_error(
        vm,
        scope,
        host,
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
    data.pending_navigation = Some(LocationNavigationRequest {
      url: resolved,
      replace,
    });
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
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let location_obj = slots
    .get(LOCATION_ACCESSOR_LOCATION_OBJ_SLOT)
    .copied()
    .and_then(|value| {
      let Value::Object(obj) = value else {
        return None;
      };
      Some(obj)
    });
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  request_location_navigation(vm, scope, host, hooks, location_obj, url_value, false)
}

fn location_href_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  request_location_navigation(vm, scope, host, hooks, Some(location_obj), url_value, false)
}

fn location_assign_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  request_location_navigation(vm, scope, host, hooks, Some(location_obj), url_value, false)
}

fn location_replace_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(location_obj) = this else {
    return Ok(Value::Undefined);
  };
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  request_location_navigation(vm, scope, host, hooks, Some(location_obj), url_value, true)
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

fn history_state_change_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let history_obj = match slots
    .get(HISTORY_OBJ_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => return Ok(Value::Undefined),
  };
  let location_obj = match slots
    .get(HISTORY_LOCATION_OBJ_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => return Ok(Value::Undefined),
  };
  let document_obj = match slots
    .get(HISTORY_DOCUMENT_OBJ_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => return Ok(Value::Undefined),
  };
  let _replace = matches!(
    slots
      .get(HISTORY_REPLACE_SLOT)
      .copied()
      .unwrap_or(Value::Undefined),
    Value::Bool(true)
  );

  // Store `history.state`. Browsers treat a missing/undefined state argument as `null`.
  let state_value = match args.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Undefined => Value::Null,
    other => other,
  };

  // The optional URL argument is resolved against the document URL (not `document.baseURI`).
  let url_value = args.get(2).copied().unwrap_or(Value::Undefined);
  if !matches!(url_value, Value::Undefined) {
    let current_document_url = {
      let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
        return Err(VmError::InvariantViolation(
          "window realm missing user data",
        ));
      };
      data.document_url.clone()
    };

    let url_value = match url_value {
      Value::String(s) => s,
      other => scope.heap_mut().to_string(other)?,
    };
    scope.push_root(Value::String(url_value))?;
    let url_input = scope.heap().get_string(url_value)?.to_utf8_lossy();

    let resolved = crate::js::url_resolve::resolve_url(&url_input, Some(&current_document_url))
      .map_err(|err| throw_type_error(vm, scope, host, hooks, &err.to_string()))?;
    let parsed = Url::parse(&resolved)
      .map_err(|err| throw_type_error(vm, scope, host, hooks, &err.to_string()))?;

    match parsed.scheme() {
      "http" | "https" | "file" | "data" | "about" => {}
      other => {
        return Err(throw_type_error(
          vm,
          scope,
          host,
          hooks,
          &format!("history state updates to {other}: URLs is not supported"),
        ));
      }
    }

    // Enforce same-origin URL updates so callers can't desync `location.origin` (currently a fixed
    // data property computed at realm init).
    //
    // For opaque origins (serialized as "null") we also require the scheme to remain stable since
    // multiple schemes share the same serialized origin.
    let current = Url::parse(&current_document_url)
      .map_err(|err| throw_type_error(vm, scope, host, hooks, &err.to_string()))?;
    let current_origin = match current.scheme() {
      "http" | "https" => current.origin().ascii_serialization(),
      _ => "null".to_string(),
    };
    let new_origin = match parsed.scheme() {
      "http" | "https" => parsed.origin().ascii_serialization(),
      _ => "null".to_string(),
    };
    if current_origin != new_origin
      || (current_origin == "null" && current.scheme() != parsed.scheme())
    {
      return Err(throw_type_error(
        vm,
        scope,
        host,
        hooks,
        "history state updates may not change origin",
      ));
    }

    // Keep the resolved URL on the location object so scripts observe updated components.
    let resolved_s = scope.alloc_string(&resolved)?;
    scope.push_root(Value::String(resolved_s))?;

    {
      // Root objects while allocating property keys: `alloc_key` can trigger GC.
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(location_obj))?;
      scope.push_root(Value::Object(document_obj))?;

      let location_url_key = alloc_key(&mut scope, LOCATION_URL_KEY)?;
      scope.define_property(
        location_obj,
        location_url_key,
        data_desc(Value::String(resolved_s)),
      )?;

      let doc_url_key = alloc_key(&mut scope, "URL")?;
      scope.define_property(
        document_obj,
        doc_url_key,
        data_desc(Value::String(resolved_s)),
      )?;
    }

    {
      let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
        return Err(VmError::InvariantViolation(
          "window realm missing user data",
        ));
      };
      let old_doc_url = std::mem::replace(&mut data.document_url, resolved.clone());
      let update_base =
        data.base_url.is_none() || data.base_url.as_deref() == Some(old_doc_url.as_str());
      if update_base {
        data.base_url = Some(resolved);
      }
    }
  }

  // Update `history.state` after URL validation so failures do not partially apply.
  {
    // Root the receiver while allocating the property key: `alloc_key` can trigger GC.
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(history_obj))?;
    scope.push_root(state_value)?;
    let state_key = alloc_key(&mut scope, "state")?;
    scope.define_property(history_obj, state_key, read_only_data_desc(state_value))?;
  }

  Ok(Value::Undefined)
}

fn history_noop_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

fn history_go_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // Per web platform behavior:
  // - history.go() / history.go(0) reloads the current document.
  // - Non-zero deltas perform a session history traversal (FastRender may implement this later).
  let delta_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut delta = scope.heap_mut().to_number(delta_value)?;
  if !delta.is_finite() || delta.is_nan() {
    delta = 0.0;
  }
  let delta = delta.trunc();
  if delta == 0.0 {
    let current_url = {
      let Some(data) = vm.user_data::<WindowRealmUserData>() else {
        return Err(VmError::InvariantViolation("window realm missing user data"));
      };
      data.document_url.clone()
    };
    {
      let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
        return Err(VmError::InvariantViolation("window realm missing user data"));
      };
      data.pending_navigation = Some(LocationNavigationRequest {
        url: current_url,
        replace: true,
      });
    }

    vm.interrupt_handle().interrupt();
    vm.tick()?;
  }

  Ok(Value::Undefined)
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

fn dom_from_vm_host(host: &mut dyn VmHost) -> Option<&dom2::Document> {
  use std::any::TypeId;

  let any = host.as_any_mut();
  let ty = any.type_id();
  let ptr = any as *mut dyn std::any::Any;

  // SAFETY: we only cast the erased `Any` pointer back to a concrete type after checking its
  // runtime `TypeId`.
  unsafe {
    if ty == TypeId::of::<crate::js::host_document::DocumentHostState>() {
      let host = &mut *(ptr as *mut crate::js::host_document::DocumentHostState);
      return Some(host.dom());
    }
    if ty == TypeId::of::<crate::api::BrowserDocumentDom2>() {
      let host = &mut *(ptr as *mut crate::api::BrowserDocumentDom2);
      return Some(host.dom());
    }
  }

  None
}

fn dom_from_vm_host_mut(host: &mut dyn VmHost) -> Option<&mut dom2::Document> {
  use std::any::TypeId;

  let any = host.as_any_mut();
  let ty = any.type_id();
  let ptr = any as *mut dyn std::any::Any;

  // SAFETY: we only cast the erased `Any` pointer back to a concrete type after checking its
  // runtime `TypeId`.
  unsafe {
    if ty == TypeId::of::<crate::js::host_document::DocumentHostState>() {
      let host = &mut *(ptr as *mut crate::js::host_document::DocumentHostState);
      return Some(host.dom_mut());
    }
    if ty == TypeId::of::<crate::api::BrowserDocumentDom2>() {
      let host = &mut *(ptr as *mut crate::api::BrowserDocumentDom2);
      return Some(host.dom_mut());
    }
  }

  None
}

/// Returns a raw pointer to the active `dom2::Document` for operations that must mutate the
/// DOM-owned event listener registry without triggering renderer invalidations.
///
/// In particular, for `BrowserDocumentDom2` we must not call `dom_mut()`, which conservatively marks
/// the document dirty (event listeners are not render-affecting).
fn dom_ptr_for_event_registry(host: &mut dyn VmHost) -> Option<NonNull<dom2::Document>> {
  use std::any::TypeId;

  let any = host.as_any_mut();
  let ty = any.type_id();
  let ptr = any as *mut dyn std::any::Any;

  // SAFETY: we only cast the erased `Any` pointer back to a concrete type after checking its
  // runtime `TypeId`.
  unsafe {
    if ty == TypeId::of::<crate::js::host_document::DocumentHostState>() {
      let host = &mut *(ptr as *mut crate::js::host_document::DocumentHostState);
      return Some(NonNull::from(host.dom_mut()));
    }
    if ty == TypeId::of::<crate::api::BrowserDocumentDom2>() {
      let host = &mut *(ptr as *mut crate::api::BrowserDocumentDom2);
      return Some(host.dom_non_null());
    }
  }

  None
}

fn dom_platform_mut(vm: &mut Vm) -> Option<&mut DomPlatform> {
  vm.user_data_mut::<WindowRealmUserData>()
    .and_then(|data| data.dom_platform.as_mut())
}
fn get_or_create_node_wrapper(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  document_obj: GcObject,
  dom: Option<&dom2::Document>,
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
    if let Some(dom) = dom {
      primary = DomInterface::primary_for_node_kind(&dom.node(node_id).kind);
    }

    platform.get_or_create_wrapper(scope, node_id, primary)?
  } else {
    scope.alloc_object()?
  };
  scope.push_root(Value::Object(wrapper))?;

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
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let src_set = {
    let key = alloc_key(scope, ELEMENT_SRC_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let srcset_get = {
    let key = alloc_key(scope, ELEMENT_SRCSET_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let srcset_set = {
    let key = alloc_key(scope, ELEMENT_SRCSET_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let sizes_get = {
    let key = alloc_key(scope, ELEMENT_SIZES_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let sizes_set = {
    let key = alloc_key(scope, ELEMENT_SIZES_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let href_get = {
    let key = alloc_key(scope, ELEMENT_HREF_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let href_set = {
    let key = alloc_key(scope, ELEMENT_HREF_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let rel_get = {
    let key = alloc_key(scope, ELEMENT_REL_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let rel_set = {
    let key = alloc_key(scope, ELEMENT_REL_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let type_get = {
    let key = alloc_key(scope, ELEMENT_TYPE_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let type_set = {
    let key = alloc_key(scope, ELEMENT_TYPE_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let charset_get = {
    let key = alloc_key(scope, ELEMENT_CHARSET_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let charset_set = {
    let key = alloc_key(scope, ELEMENT_CHARSET_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let cross_origin_get = {
    let key = alloc_key(scope, ELEMENT_CROSS_ORIGIN_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let cross_origin_set = {
    let key = alloc_key(scope, ELEMENT_CROSS_ORIGIN_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let async_get = {
    let key = alloc_key(scope, ELEMENT_ASYNC_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let async_set = {
    let key = alloc_key(scope, ELEMENT_ASYNC_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let defer_get = {
    let key = alloc_key(scope, ELEMENT_DEFER_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let defer_set = {
    let key = alloc_key(scope, ELEMENT_DEFER_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let height_get = {
    let key = alloc_key(scope, ELEMENT_HEIGHT_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let height_set = {
    let key = alloc_key(scope, ELEMENT_HEIGHT_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let width_get = {
    let key = alloc_key(scope, ELEMENT_WIDTH_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let width_set = {
    let key = alloc_key(scope, ELEMENT_WIDTH_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
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
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_set_property = {
    let key = alloc_key(scope, STYLE_SET_PROPERTY_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_remove_property = {
    let key = alloc_key(scope, STYLE_REMOVE_PROPERTY_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_css_text_get = {
    let key = alloc_key(scope, STYLE_CSS_TEXT_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_css_text_set = {
    let key = alloc_key(scope, STYLE_CSS_TEXT_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_display_get = {
    let key = alloc_key(scope, STYLE_DISPLAY_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_display_set = {
    let key = alloc_key(scope, STYLE_DISPLAY_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_cursor_get = {
    let key = alloc_key(scope, STYLE_CURSOR_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_cursor_set = {
    let key = alloc_key(scope, STYLE_CURSOR_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_height_get = {
    let key = alloc_key(scope, STYLE_HEIGHT_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_height_set = {
    let key = alloc_key(scope, STYLE_HEIGHT_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_width_get = {
    let key = alloc_key(scope, STYLE_WIDTH_GET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
  };
  let style_width_set = {
    let key = alloc_key(scope, STYLE_WIDTH_SET_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
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

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (cross_origin_get, cross_origin_set)
  {
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

  let is_element_like = dom
    .map(|dom| {
      matches!(
        dom.node(node_id).kind,
        dom2::NodeKind::Element { .. } | dom2::NodeKind::Slot { .. }
      )
    })
    .unwrap_or(false);

  if is_element_like {
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

      let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
      scope.define_property(
        class_list,
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

    // `Element.dataset` (DOMStringMap-like): implemented via host exotic property hooks so
    // `el.dataset.fooBar = "x"` reflects to `data-foo-bar="x"`.
    let dataset = scope.alloc_object()?;
    scope.push_root(Value::Object(dataset))?;
    scope.heap_mut().object_set_host_slots(
      dataset,
      HostSlots {
        a: node_id.index() as u64,
        b: DOM_STRING_MAP_HOST_KIND,
      },
    )?;

    let key = alloc_key(scope, "dataset")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(dataset)))?;

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

      let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
      scope.define_property(
        style,
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

      let get_property_value_key = alloc_key(scope, "getPropertyValue")?;
      scope.define_property(
        style,
        get_property_value_key,
        data_desc(Value::Object(get_property_value)),
      )?;

      let set_property_key = alloc_key(scope, "setProperty")?;
      scope.define_property(
        style,
        set_property_key,
        data_desc(Value::Object(set_property)),
      )?;

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

  if let (Some(Value::Object(get)), Some(Value::Object(set))) = (text_content_get, text_content_set)
  {
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
  document_obj: GcObject,
  dom: &dom2::Document,
  node_id: NodeId,
  array: GcObject,
) -> Result<(), VmError> {
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
  let old_len = match scope
    .heap()
    .object_get_own_data_property_value(array, &length_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => 0,
  };

  // Overwrite / populate indices.
  let mut idx_buf = [0u8; 20];
  for (idx, child_id) in children.iter().copied().enumerate() {
    let idx_str = decimal_str_for_usize(idx, &mut idx_buf);
    let key = alloc_key(scope, idx_str)?;
    let wrapper = get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), child_id)?;
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
  document_obj: GcObject,
  dom: &dom2::Document,
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
  sync_child_nodes_array(vm, scope, document_obj, dom, node_id, array)
}

fn sync_cached_child_nodes_for_node_id(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  document_obj: GcObject,
  dom: &dom2::Document,
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
  sync_cached_child_nodes_for_wrapper(vm, scope, document_obj, dom, wrapper_obj, node_id)
}

fn document_window_from_document(
  scope: &mut Scope<'_>,
  document_obj: GcObject,
) -> Result<Option<GcObject>, VmError> {
  scope.push_root(Value::Object(document_obj))?;
  let key = alloc_key(scope, DOCUMENT_WINDOW_KEY)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      }),
  )
}

fn queue_mutation_observer_microtask(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  document_obj: GcObject,
) -> Result<(), VmError> {
  if !hooks_have_event_loop(hooks) {
    return Ok(());
  }

  let Some(global) = document_window_from_document(scope, document_obj)? else {
    return Ok(());
  };

  // Find the internal queueMicrotask implementation (preferred) or fall back to the user-visible
  // `queueMicrotask` binding.
  let queue_microtask = {
    scope.push_root(Value::Object(global))?;
    let key = alloc_key(scope, INTERNAL_QUEUE_MICROTASK_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)?
      .or_else(|| {
        let key = alloc_key(scope, "queueMicrotask").ok()?;
        scope
          .heap()
          .object_get_own_data_property_value(global, &key)
          .ok()
          .flatten()
      })
      .unwrap_or(Value::Undefined)
  };

  if !matches!(queue_microtask, Value::Object(_)) || !scope.heap().is_callable(queue_microtask)? {
    return Ok(());
  }

  let notify = {
    scope.push_root(Value::Object(document_obj))?;
    let notify_key = alloc_key(scope, MUTATION_OBSERVER_NOTIFY_KEY)?;
    scope
      .heap()
      .object_get_own_data_property_value(document_obj, &notify_key)?
      .unwrap_or(Value::Undefined)
  };

  if !matches!(notify, Value::Object(_)) || !scope.heap().is_callable(notify)? {
    return Ok(());
  }

  // This is an internal scheduling primitive; don't let failures (missing queueMicrotask binding,
  // full microtask queue, etc) perturb DOM mutations.
  let _ = vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    queue_microtask,
    Value::Undefined,
    &[notify],
  );
  Ok(())
}

fn maybe_queue_mutation_observer_microtask(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  document_obj: GcObject,
  needs_microtask: bool,
) -> Result<(), VmError> {
  if needs_microtask {
    queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj)?;
  }
  Ok(())
}

fn document_document_element_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::Null);
  };
  let Some(node_id) = dom.document_element() else {
    return Ok(Value::Null);
  };

  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)
}

fn document_head_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::Null);
  };
  let Some(node_id) = dom.head() else {
    return Ok(Value::Null);
  };

  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)
}

fn document_body_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::Null);
  };
  let Some(node_id) = dom.body() else {
    return Ok(Value::Null);
  };

  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)
}

fn document_get_element_by_id_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  // Brand check: `Document.prototype.getElementById` must only be callable on real Document
  // wrappers, not arbitrary DOM-backed nodes (e.g. Elements) that happen to have a source id.
  {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let _ = platform.require_document_id(scope.heap(), Value::Object(document_obj))?;
  };
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

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
  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)
}

fn document_fragment_get_element_by_id_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let fragment_id = {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    platform.require_document_fragment_id(scope.heap(), Value::Object(wrapper_obj))?
  };
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

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
  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), found)
}

fn document_query_selector_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let _ = platform.require_document_id(scope.heap(), Value::Object(document_obj))?;
  };
  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError("Illegal invocation"))?;

  let selector_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let selector_value = scope.heap_mut().to_string(selector_value)?;
  let selector = scope
    .heap()
    .get_string(selector_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  match dom.query_selector(&selector, None) {
    Ok(Some(node_id)) => get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id),
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
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  {
    let platform = dom_platform_mut(vm).ok_or(VmError::TypeError("Illegal invocation"))?;
    let _ = platform.require_document_id(scope.heap(), Value::Object(document_obj))?;
  };
  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError("Illegal invocation"))?;

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
    let wrapper = get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)?;
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
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.querySelector requires a DOM-backed element",
  ))?;
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
    Ok(Some(found)) => get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), found),
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
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.querySelectorAll requires a DOM-backed element",
  ))?;
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
    let wrapper = get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)?;
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
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.matches requires a DOM-backed element",
  ))?;
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
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.closest requires a DOM-backed element",
  ))?;
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
    Ok(Some(found)) => get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), found),
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
  host: &mut dyn VmHost,
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

  let tag_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let tag_value = scope.heap_mut().to_string(tag_value)?;
  let tag_name = scope
    .heap()
    .get_string(tag_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "document.createElement requires a DOM-backed document",
  ))?;
  let node_id = dom.create_element(&tag_name, "");

  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)
}

fn document_create_text_node_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let data_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let data_value = scope.heap_mut().to_string(data_value)?;
  let data = scope
    .heap()
    .get_string(data_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "document.createTextNode requires a DOM-backed document",
  ))?;
  let node_id = dom.create_text(&data);

  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)
}

fn document_create_comment_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let data_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let data_value = scope.heap_mut().to_string(data_value)?;
  let data = scope
    .heap()
    .get_string(data_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "document.createComment requires a DOM-backed document",
  ))?;
  let node_id = dom.create_comment(&data);

  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)
}

fn document_create_document_fragment_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "document.createDocumentFragment requires a DOM-backed document",
  ))?;
  let node_id = dom.create_document_fragment();

  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), node_id)
}

fn event_constructor_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "Event constructor cannot be invoked without 'new'",
  ))
}

fn event_constructor_impl(
  scope: &mut Scope<'_>,
  ctor: GcObject,
  args: &[Value],
) -> Result<Value, VmError> {
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
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "CustomEvent constructor cannot be invoked without 'new'",
  ))
}

fn custom_event_constructor_impl(
  scope: &mut Scope<'_>,
  ctor: GcObject,
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

fn storage_event_constructor_native(
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
  let mut key = Value::Null;
  let mut old_value = Value::Null;
  let mut new_value = Value::Null;
  let mut url = Value::String(scope.alloc_string("")?);
  let mut storage_area = Value::Null;

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

      let key_key = alloc_key(scope, "key")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &key_key)?
      {
        key = match value {
          Value::Undefined => Value::Null,
          Value::Null => Value::Null,
          other => Value::String(scope.heap_mut().to_string(other)?),
        };
      }

      let old_value_key = alloc_key(scope, "oldValue")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &old_value_key)?
      {
        old_value = match value {
          Value::Undefined => Value::Null,
          Value::Null => Value::Null,
          other => Value::String(scope.heap_mut().to_string(other)?),
        };
      }

      let new_value_key = alloc_key(scope, "newValue")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &new_value_key)?
      {
        new_value = match value {
          Value::Undefined => Value::Null,
          Value::Null => Value::Null,
          other => Value::String(scope.heap_mut().to_string(other)?),
        };
      }

      let url_key = alloc_key(scope, "url")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &url_key)?
      {
        if !matches!(value, Value::Undefined) {
          url = Value::String(scope.heap_mut().to_string(value)?);
        }
      }

      let storage_area_key = alloc_key(scope, "storageArea")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &storage_area_key)?
      {
        if !matches!(value, Value::Undefined) {
          storage_area = value;
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

  // StorageEvent fields.
  let key_key = alloc_key(scope, "key")?;
  scope.define_property(obj, key_key, read_only_data_desc(key))?;

  let old_value_key = alloc_key(scope, "oldValue")?;
  scope.define_property(obj, old_value_key, read_only_data_desc(old_value))?;

  let new_value_key = alloc_key(scope, "newValue")?;
  scope.define_property(obj, new_value_key, read_only_data_desc(new_value))?;

  let url_key = alloc_key(scope, "url")?;
  scope.define_property(obj, url_key, read_only_data_desc(url))?;

  let storage_area_key = alloc_key(scope, "storageArea")?;
  scope.define_property(obj, storage_area_key, read_only_data_desc(storage_area))?;

  Ok(Value::Object(obj))
}

fn promise_rejection_event_constructor_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "PromiseRejectionEvent constructor cannot be invoked without 'new'",
  ))
}

fn promise_rejection_event_constructor_impl(
  scope: &mut Scope<'_>,
  ctor: GcObject,
  args: &[Value],
) -> Result<Value, VmError> {
  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_string = scope.heap_mut().to_string(type_arg)?;

  let init_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let Value::Object(init_obj) = init_value else {
    return Err(VmError::TypeError(
      "PromiseRejectionEvent constructor requires an eventInitDict",
    ));
  };

  let bubbles_key = alloc_key(scope, "bubbles")?;
  let cancelable_key = alloc_key(scope, "cancelable")?;
  let composed_key = alloc_key(scope, "composed")?;
  let promise_key = alloc_key(scope, "promise")?;
  let reason_key = alloc_key(scope, "reason")?;

  let bubbles = scope
    .heap()
    .object_get_own_data_property_value(init_obj, &bubbles_key)?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?
    .unwrap_or(false);
  let cancelable = scope
    .heap()
    .object_get_own_data_property_value(init_obj, &cancelable_key)?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?
    .unwrap_or(false);
  let composed = scope
    .heap()
    .object_get_own_data_property_value(init_obj, &composed_key)?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?
    .unwrap_or(false);

  let promise_value = scope
    .heap()
    .object_get_own_data_property_value(init_obj, &promise_key)?
    .unwrap_or(Value::Undefined);
  let Value::Object(promise_obj) = promise_value else {
    return Err(VmError::TypeError(
      "PromiseRejectionEventInit.promise must be an object",
    ));
  };

  let reason = scope
    .heap()
    .object_get_own_data_property_value(init_obj, &reason_key)?
    .unwrap_or(Value::Undefined);

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

  let type_key = alloc_key(scope, "type")?;
  scope.define_property(obj, type_key, data_desc(Value::String(type_string)))?;
  scope.define_property(obj, bubbles_key, data_desc(Value::Bool(bubbles)))?;
  scope.define_property(obj, cancelable_key, data_desc(Value::Bool(cancelable)))?;
  scope.define_property(obj, composed_key, data_desc(Value::Bool(composed)))?;

  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  scope.define_property(obj, default_prevented_key, data_desc(Value::Bool(false)))?;
  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(obj, cancel_bubble_key, data_desc(Value::Bool(false)))?;

  scope.define_property(
    obj,
    promise_key,
    read_only_data_desc(Value::Object(promise_obj)),
  )?;
  scope.define_property(obj, reason_key, read_only_data_desc(reason))?;

  Ok(Value::Object(obj))
}

fn error_event_constructor_native(
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
  let mut message = scope.alloc_string("")?;
  let mut filename = scope.alloc_string("")?;
  let mut lineno = 0u32;
  let mut colno = 0u32;
  let mut error = Value::Undefined;

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

      let message_key = alloc_key(scope, "message")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &message_key)?
      {
        message = scope.heap_mut().to_string(value)?;
      }

      let filename_key = alloc_key(scope, "filename")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &filename_key)?
      {
        filename = scope.heap_mut().to_string(value)?;
      }

      let lineno_key = alloc_key(scope, "lineno")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &lineno_key)?
      {
        let n = scope.heap_mut().to_number(value)?;
        lineno = if n.is_finite() && n >= 0.0 {
          n.floor().min(u32::MAX as f64) as u32
        } else {
          0
        };
      }

      let colno_key = alloc_key(scope, "colno")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &colno_key)?
      {
        let n = scope.heap_mut().to_number(value)?;
        colno = if n.is_finite() && n >= 0.0 {
          n.floor().min(u32::MAX as f64) as u32
        } else {
          0
        };
      }

      let error_key = alloc_key(scope, "error")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &error_key)?
      {
        error = value;
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

  let message_key = alloc_key(scope, "message")?;
  scope.define_property(obj, message_key, read_only_data_desc(Value::String(message)))?;

  let filename_key = alloc_key(scope, "filename")?;
  scope.define_property(obj, filename_key, read_only_data_desc(Value::String(filename)))?;

  let lineno_key = alloc_key(scope, "lineno")?;
  scope.define_property(
    obj,
    lineno_key,
    read_only_data_desc(Value::Number(lineno as f64)),
  )?;

  let colno_key = alloc_key(scope, "colno")?;
  scope.define_property(
    obj,
    colno_key,
    read_only_data_desc(Value::Number(colno as f64)),
  )?;

  let error_key = alloc_key(scope, "error")?;
  scope.define_property(obj, error_key, read_only_data_desc(error))?;

  Ok(Value::Object(obj))
}

fn before_unload_event_constructor_native(
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
  let mut return_value = scope.alloc_string("")?;

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

      let return_value_key = alloc_key(scope, "returnValue")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &return_value_key)?
      {
        return_value = scope.heap_mut().to_string(value)?;
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

  let return_value_key = alloc_key(scope, "returnValue")?;
  scope.define_property(
    obj,
    return_value_key,
    data_desc(Value::String(return_value)),
  )?;

  Ok(Value::Object(obj))
}

fn page_transition_event_constructor_native(
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
  let mut persisted = false;

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

      let persisted_key = alloc_key(scope, "persisted")?;
      if let Some(value) = scope
        .heap()
        .object_get_own_data_property_value(init_obj, &persisted_key)?
      {
        persisted = scope.heap().to_boolean(value)?;
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

  let persisted_key = alloc_key(scope, "persisted")?;
  scope.define_property(
    obj,
    persisted_key,
    read_only_data_desc(Value::Bool(persisted)),
  )?;

  Ok(Value::Object(obj))
}

fn event_constructor_construct_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };
  event_constructor_impl(scope, ctor, args)
}

fn custom_event_constructor_construct_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };
  custom_event_constructor_impl(scope, ctor, args)
}

fn storage_event_constructor_construct_native(
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
  storage_event_constructor_native(vm, scope, host, hooks, ctor, Value::Undefined, args)
}

fn promise_rejection_event_constructor_construct_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };
  promise_rejection_event_constructor_impl(scope, ctor, args)
}

fn error_event_constructor_construct_native(
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
  error_event_constructor_native(vm, scope, host, hooks, ctor, Value::Undefined, args)
}

fn before_unload_event_constructor_construct_native(
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
  before_unload_event_constructor_native(vm, scope, host, hooks, ctor, Value::Undefined, args)
}

fn page_transition_event_constructor_construct_native(
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
  page_transition_event_constructor_native(vm, scope, host, hooks, ctor, Value::Undefined, args)
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

fn storage_event_init_storage_event_native(
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
      "StorageEvent.initStorageEvent must be called on a StorageEvent object",
    ));
  };

  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_string = scope.heap_mut().to_string(type_arg)?;

  let bubbles_arg = args.get(1).copied().unwrap_or(Value::Undefined);
  let bubbles = scope.heap().to_boolean(bubbles_arg)?;

  let cancelable_arg = args.get(2).copied().unwrap_or(Value::Undefined);
  let cancelable = scope.heap().to_boolean(cancelable_arg)?;

  let key_arg = args.get(3).copied().unwrap_or(Value::Undefined);
  let key = match key_arg {
    Value::Undefined => Value::Null,
    Value::Null => Value::Null,
    other => Value::String(scope.heap_mut().to_string(other)?),
  };

  let old_value_arg = args.get(4).copied().unwrap_or(Value::Undefined);
  let old_value = match old_value_arg {
    Value::Undefined => Value::Null,
    Value::Null => Value::Null,
    other => Value::String(scope.heap_mut().to_string(other)?),
  };

  let new_value_arg = args.get(5).copied().unwrap_or(Value::Undefined);
  let new_value = match new_value_arg {
    Value::Undefined => Value::Null,
    Value::Null => Value::Null,
    other => Value::String(scope.heap_mut().to_string(other)?),
  };

  let url_arg = args.get(6).copied().unwrap_or(Value::Undefined);
  let url = if matches!(url_arg, Value::Undefined) {
    Value::String(scope.alloc_string("")?)
  } else {
    Value::String(scope.heap_mut().to_string(url_arg)?)
  };

  let storage_area_arg = args.get(7).copied().unwrap_or(Value::Undefined);
  let storage_area = if matches!(storage_area_arg, Value::Undefined) {
    Value::Null
  } else {
    storage_area_arg
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

  // `initStorageEvent` does not expose `composed`; reset to false per DOM.
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

  let key_key = alloc_key(scope, "key")?;
  scope.define_property(event_obj, key_key, read_only_data_desc(key))?;

  let old_value_key = alloc_key(scope, "oldValue")?;
  scope.define_property(event_obj, old_value_key, read_only_data_desc(old_value))?;

  let new_value_key = alloc_key(scope, "newValue")?;
  scope.define_property(event_obj, new_value_key, read_only_data_desc(new_value))?;

  let url_key = alloc_key(scope, "url")?;
  scope.define_property(event_obj, url_key, read_only_data_desc(url))?;

  let storage_area_key = alloc_key(scope, "storageArea")?;
  scope.define_property(
    event_obj,
    storage_area_key,
    read_only_data_desc(storage_area),
  )?;

  Ok(Value::Undefined)
}

fn event_prototype_prevent_default_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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
    if let Some(default_prevented) = with_active_event_for_host(host, event_id, |event| {
      event.prevent_default();
      event.default_prevented
    }) {
      let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
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
  host: &mut dyn VmHost,
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
    if with_active_event_for_host(host, event_id, |event| event.stop_propagation()).is_some() {
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
  host: &mut dyn VmHost,
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
    if with_active_event_for_host(host, event_id, |event| event.stop_immediate_propagation())
      .is_some()
    {
      let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
      scope.define_property(event_obj, cancel_bubble_key, data_desc(Value::Bool(true)))?;
      let immediate_key = alloc_key(scope, EVENT_IMMEDIATE_STOP_KEY)?;
      scope.define_property(event_obj, immediate_key, data_desc(Value::Bool(true)))?;
      return Ok(Value::Undefined);
    }
  }

  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  scope.define_property(event_obj, cancel_bubble_key, data_desc(Value::Bool(true)))?;
  let immediate_key = alloc_key(scope, EVENT_IMMEDIATE_STOP_KEY)?;
  scope.define_property(event_obj, immediate_key, data_desc(Value::Bool(true)))?;
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
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
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

  // FastRender-only extension: allow `new EventTarget(parent)` so WPT fixtures can build a manual
  // event propagation chain without DOM nodes.
  let parent_value = args.get(0).copied().unwrap_or(Value::Undefined);
  if !matches!(parent_value, Value::Undefined | Value::Null) {
    let Value::Object(parent_obj) = parent_value else {
      return Err(VmError::TypeError(
        "EventTarget parent must be an EventTarget object",
      ));
    };
    if !is_branded_event_target(scope, parent_obj)? {
      return Err(VmError::TypeError(
        "EventTarget parent must be an EventTarget object",
      ));
    }

    let parent_key = alloc_key(scope, EVENT_TARGET_PARENT_KEY)?;
    scope.push_root(Value::Object(parent_obj))?;
    scope.define_property(
      obj,
      parent_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Object(parent_obj),
          writable: false,
        },
      },
    )?;
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

  let child_id = gc_object_id(obj);

  // Register this opaque EventTarget so dispatch can resolve `event.target/currentTarget` back into
  // a JS object, and so we can locate per-target callback roots.
  if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
    if let Some(dom) = dom_from_vm_host(host) {
      dom
        .events()
        .register_opaque_target(child_id, vm_js::WeakGcObject::new(obj));
    } else {
      data
        .events_dom_fallback
        .events()
        .register_opaque_target(child_id, vm_js::WeakGcObject::new(obj));
    }
  }

  // Non-standard extension used by curated WPT tests:
  // `new EventTarget(parent)` attaches an explicit parent so capture/bubble can traverse a synthetic
  // chain (useful for exercising the dispatch algorithm without real DOM nodes).
  if let Some(parent_value) = args.get(0).copied() {
    match parent_value {
      Value::Undefined | Value::Null => {}
      Value::Object(parent_obj) => {
        let parent_target = resolve_dom_event_target(vm, scope, host, parent_obj)
          .ok()
          .map(|(resolved, _dom_ptr)| resolved.target_id)
          .or_else(|| {
            is_branded_event_target(scope, parent_obj)
              .ok()
              .and_then(|is_branded| {
                is_branded.then_some(web_events::EventTargetId::Opaque(gc_object_id(parent_obj)))
              })
          });

        let Some(parent_target) = parent_target else {
          return Err(VmError::TypeError(
            "EventTarget parent must be an EventTarget",
          ));
        };

        if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
          if let Some(dom) = dom_from_vm_host(host) {
            dom
              .events()
              .set_opaque_parent(child_id, Some(parent_target));
          } else {
            data
              .events_dom_fallback
              .events()
              .set_opaque_parent(child_id, Some(parent_target));
          }
        }
      }
      _ => {
        return Err(VmError::TypeError(
          "EventTarget parent must be an EventTarget",
        ))
      }
    }
  }
  Ok(Value::Object(obj))
}

fn mutation_observer_constructor_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "MutationObserver constructor cannot be invoked without 'new'",
  ))
}

fn mutation_observer_constructor_construct_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if !matches!(callback, Value::Object(_)) || !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError(
      "MutationObserver constructor requires a callable callback",
    ));
  }

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

  let id = NEXT_MUTATION_OBSERVER_ID.fetch_add(1, Ordering::Relaxed);

  let id_key = alloc_key(scope, MUTATION_OBSERVER_ID_KEY)?;
  scope.define_property(
    obj,
    id_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(id as f64),
        writable: false,
      },
    },
  )?;

  let callback_key = alloc_key(scope, MUTATION_OBSERVER_CALLBACK_KEY)?;
  scope.define_property(
    obj,
    callback_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: callback,
        writable: false,
      },
    },
  )?;

  Ok(Value::Object(obj))
}

fn mutation_observer_id_from_obj(scope: &mut Scope<'_>, obj: GcObject) -> Result<u64, VmError> {
  scope.push_root(Value::Object(obj))?;
  let id_key = alloc_key(scope, MUTATION_OBSERVER_ID_KEY)?;
  match scope
    .heap()
    .object_get_own_data_property_value(obj, &id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => Ok(n as u64),
    _ => Err(VmError::TypeError("Illegal invocation")),
  }
}

fn mutation_observer_document_from_obj(
  scope: &mut Scope<'_>,
  obj: GcObject,
) -> Result<Option<GcObject>, VmError> {
  scope.push_root(Value::Object(obj))?;
  let key = alloc_key(scope, MUTATION_OBSERVER_DOCUMENT_KEY)?;
  Ok(
    match scope.heap().object_get_own_data_property_value(obj, &key)? {
      Some(Value::Object(doc)) => Some(doc),
      _ => None,
    },
  )
}

fn mutation_observer_parse_options(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  options_value: Value,
) -> Result<dom2::MutationObserverInit, VmError> {
  let options_obj = match options_value {
    Value::Undefined => None,
    Value::Object(obj) => Some(obj),
    _ => {
      return Err(VmError::TypeError(
        "MutationObserver.observe: options must be an object",
      ));
    }
  };

  let mut options = dom2::MutationObserverInit::default();

  if let Some(obj) = options_obj {
    scope.push_root(Value::Object(obj))?;

    let mut get_bool_opt = |vm: &mut Vm,
                            scope: &mut Scope<'_>,
                            obj: GcObject,
                            name: &str|
     -> Result<Option<bool>, VmError> {
      let key = alloc_key(scope, name)?;
      let v = vm.get_with_host_and_hooks(host, scope, hooks, obj, key)?;
      if matches!(v, Value::Undefined) {
        return Ok(None);
      }
      Ok(Some(scope.heap().to_boolean(v)?))
    };

    options.child_list = get_bool_opt(vm, scope, obj, "childList")?.unwrap_or(false);
    let mut attributes = get_bool_opt(vm, scope, obj, "attributes")?;
    let mut character_data = get_bool_opt(vm, scope, obj, "characterData")?;
    options.subtree = get_bool_opt(vm, scope, obj, "subtree")?.unwrap_or(false);
    let attribute_old_value = get_bool_opt(vm, scope, obj, "attributeOldValue")?;
    let character_data_old_value = get_bool_opt(vm, scope, obj, "characterDataOldValue")?;

    let attr_filter_key = alloc_key(scope, "attributeFilter")?;
    let filter_value = vm.get_with_host_and_hooks(host, scope, hooks, obj, attr_filter_key)?;
    let mut attribute_filter: Option<Vec<String>> = None;
    if !matches!(filter_value, Value::Undefined) {
      let Value::Object(filter_obj) = filter_value else {
        return Err(VmError::TypeError(
          "MutationObserver.observe: attributeFilter must be an array",
        ));
      };
      scope.push_root(Value::Object(filter_obj))?;

      let length_key = alloc_key(scope, "length")?;
      let length_value = vm.get_with_host_and_hooks(host, scope, hooks, filter_obj, length_key)?;
      let len = match length_value {
        Value::Number(n) if n.is_finite() && n >= 0.0 => (n.trunc() as usize).min(1024),
        _ => 0,
      };

      let mut out: Vec<String> = Vec::with_capacity(len.min(32));
      for idx in 0..len {
        let idx_key = alloc_key(scope, &idx.to_string())?;
        let v = vm.get_with_host_and_hooks(host, scope, hooks, filter_obj, idx_key)?;
        let s = scope.heap_mut().to_string(v)?;
        let text = scope
          .heap()
          .get_string(s)
          .map(|s| s.to_utf8_lossy())
          .unwrap_or_default();
        out.push(text);
      }
      attribute_filter = Some(out);
    }

    // DOM: attributeOldValue / attributeFilter imply `attributes` when the `attributes` member is
    // absent. Similarly, `characterDataOldValue` implies `characterData` when absent.
    if (attribute_old_value.is_some() || attribute_filter.is_some()) && attributes.is_none() {
      attributes = Some(true);
    }
    if character_data_old_value.is_some() && character_data.is_none() {
      character_data = Some(true);
    }

    options.attributes = attributes.unwrap_or(false);
    options.character_data = character_data.unwrap_or(false);
    options.attribute_old_value = attribute_old_value.unwrap_or(false);
    options.character_data_old_value = character_data_old_value.unwrap_or(false);
    options.attribute_filter = attribute_filter;
  }

  if !options.child_list && !options.attributes && !options.character_data {
    return Err(VmError::TypeError(
      "MutationObserver.observe: at least one of childList, attributes, or characterData must be true",
    ));
  }
  if options.attribute_old_value && !options.attributes {
    return Err(VmError::TypeError(
      "MutationObserver.observe: attributeOldValue requires attributes",
    ));
  }
  if options.attribute_filter.is_some() && !options.attributes {
    return Err(VmError::TypeError(
      "MutationObserver.observe: attributeFilter requires attributes",
    ));
  }
  if options.character_data_old_value && !options.character_data {
    return Err(VmError::TypeError(
      "MutationObserver.observe: characterDataOldValue requires characterData",
    ));
  }

  Ok(options)
}

fn mutation_observer_registry_from_document(
  scope: &mut Scope<'_>,
  document_obj: GcObject,
) -> Result<GcObject, VmError> {
  scope.push_root(Value::Object(document_obj))?;
  let key = alloc_key(scope, MUTATION_OBSERVER_REGISTRY_KEY)?;
  match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &key)?
  {
    Some(Value::Object(obj)) => Ok(obj),
    _ => {
      let registry = scope.alloc_object()?;
      scope.push_root(Value::Object(registry))?;
      scope.define_property(
        document_obj,
        key,
        PropertyDescriptor {
          enumerable: false,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Object(registry),
            writable: false,
          },
        },
      )?;
      Ok(registry)
    }
  }
}

fn alloc_node_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  document_obj: GcObject,
  dom: Option<&dom2::Document>,
  nodes: &[NodeId],
) -> Result<GcObject, VmError> {
  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  if let Some(intrinsics) = vm.intrinsics() {
    scope
      .heap_mut()
      .object_set_prototype(array, Some(intrinsics.array_prototype()))?;
  }

  for (idx, node_id) in nodes.iter().copied().enumerate() {
    let key = alloc_key(scope, &idx.to_string())?;
    let wrapper = get_or_create_node_wrapper(vm, scope, document_obj, dom, node_id)?;
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
        value: Value::Number(nodes.len() as f64),
        writable: true,
      },
    },
  )?;

  Ok(array)
}

fn alloc_mutation_record_object(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  document_obj: GcObject,
  dom: Option<&dom2::Document>,
  record: &dom2::MutationRecord,
) -> Result<GcObject, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  let type_key = alloc_key(scope, "type")?;
  let type_s = scope.alloc_string(record.type_.as_str())?;
  scope.push_root(Value::String(type_s))?;
  scope.define_property(obj, type_key, data_desc(Value::String(type_s)))?;

  let target_key = alloc_key(scope, "target")?;
  let target_wrapper = get_or_create_node_wrapper(vm, scope, document_obj, dom, record.target)?;
  scope.define_property(obj, target_key, data_desc(target_wrapper))?;

  let added_key = alloc_key(scope, "addedNodes")?;
  let added = alloc_node_array(vm, scope, document_obj, dom, &record.added_nodes)?;
  scope.define_property(obj, added_key, data_desc(Value::Object(added)))?;

  let removed_key = alloc_key(scope, "removedNodes")?;
  let removed = alloc_node_array(vm, scope, document_obj, dom, &record.removed_nodes)?;
  scope.define_property(obj, removed_key, data_desc(Value::Object(removed)))?;

  let prev_key = alloc_key(scope, "previousSibling")?;
  let prev = match record.previous_sibling {
    Some(id) => get_or_create_node_wrapper(vm, scope, document_obj, dom, id)?,
    None => Value::Null,
  };
  scope.define_property(obj, prev_key, data_desc(prev))?;

  let next_key = alloc_key(scope, "nextSibling")?;
  let next = match record.next_sibling {
    Some(id) => get_or_create_node_wrapper(vm, scope, document_obj, dom, id)?,
    None => Value::Null,
  };
  scope.define_property(obj, next_key, data_desc(next))?;

  let attr_name_key = alloc_key(scope, "attributeName")?;
  let attr_name = match record.attribute_name.as_deref() {
    Some(name) => {
      let s = scope.alloc_string(name)?;
      scope.push_root(Value::String(s))?;
      Value::String(s)
    }
    None => Value::Null,
  };
  scope.define_property(obj, attr_name_key, data_desc(attr_name))?;

  let attr_ns_key = alloc_key(scope, "attributeNamespace")?;
  scope.define_property(obj, attr_ns_key, data_desc(Value::Null))?;

  let old_value_key = alloc_key(scope, "oldValue")?;
  let old_value = match record.old_value.as_deref() {
    Some(value) => {
      let s = scope.alloc_string(value)?;
      scope.push_root(Value::String(s))?;
      Value::String(s)
    }
    None => Value::Null,
  };
  scope.define_property(obj, old_value_key, data_desc(old_value))?;

  Ok(obj)
}

fn alloc_mutation_records_array(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  document_obj: GcObject,
  dom: Option<&dom2::Document>,
  records: &[dom2::MutationRecord],
) -> Result<GcObject, VmError> {
  let array = scope.alloc_array(0)?;
  scope.push_root(Value::Object(array))?;
  if let Some(intrinsics) = vm.intrinsics() {
    scope
      .heap_mut()
      .object_set_prototype(array, Some(intrinsics.array_prototype()))?;
  }

  for (idx, record) in records.iter().enumerate() {
    let key = alloc_key(scope, &idx.to_string())?;
    let obj = alloc_mutation_record_object(vm, scope, document_obj, dom, record)?;
    scope.define_property(array, key, data_desc(Value::Object(obj)))?;
  }

  let length_key = alloc_key(scope, "length")?;
  scope.define_property(
    array,
    length_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(records.len() as f64),
        writable: true,
      },
    },
  )?;
  Ok(array)
}

fn mutation_observer_observe_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(observer_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let observer_id = mutation_observer_id_from_obj(scope, observer_obj)?;

  let target_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(target_obj) = target_value else {
    return Err(VmError::TypeError(
      "MutationObserver.observe: target must be a Node",
    ));
  };

  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(target_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "MutationObserver.observe: target must be a Node",
      ));
    }
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(target_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => {
      return Err(VmError::TypeError(
        "MutationObserver.observe: target must be a Node",
      ));
    }
  };

  let options_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let options = mutation_observer_parse_options(vm, scope, host, hooks, options_value)?;

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "MutationObserver.observe requires a DOM-backed node",
  ))?;
  let target_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("MutationObserver.observe requires a DOM-backed node"))?;

  if let Err(err) = dom.mutation_observer_observe(observer_id, target_id, options) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  // Ensure the observer stays alive while it is observing.
  let registry = mutation_observer_registry_from_document(scope, document_obj)?;
  scope.push_root(Value::Object(registry))?;
  scope.push_root(Value::Object(observer_obj))?;
  let key = alloc_key(scope, &observer_id.to_string())?;
  scope.define_property(registry, key, data_desc(Value::Object(observer_obj)))?;

  // Remember which document this observer is associated with so `disconnect()`/`takeRecords()` can
  // find the right `dom2::Document`.
  let doc_key = alloc_key(scope, MUTATION_OBSERVER_DOCUMENT_KEY)?;
  scope.define_property(
    observer_obj,
    doc_key,
    data_desc(Value::Object(document_obj)),
  )?;

  Ok(Value::Undefined)
}

fn mutation_observer_disconnect_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(observer_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let observer_id = mutation_observer_id_from_obj(scope, observer_obj)?;

  let Some(document_obj) = mutation_observer_document_from_obj(scope, observer_obj)? else {
    return Ok(Value::Undefined);
  };

  if let Some(dom) = dom_from_vm_host_mut(host) {
    dom.mutation_observer_disconnect(observer_id);
  };

  if let Ok(registry) = mutation_observer_registry_from_document(scope, document_obj) {
    scope.push_root(Value::Object(registry))?;
    let key = alloc_key(scope, &observer_id.to_string())?;
    scope.define_property(registry, key, data_desc(Value::Undefined))?;
  }

  Ok(Value::Undefined)
}

fn mutation_observer_take_records_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(observer_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let observer_id = mutation_observer_id_from_obj(scope, observer_obj)?;

  let alloc_empty_array = |vm: &mut Vm, scope: &mut Scope<'_>| -> Result<GcObject, VmError> {
    let array = scope.alloc_array(0)?;
    scope.push_root(Value::Object(array))?;
    if let Some(intrinsics) = vm.intrinsics() {
      scope
        .heap_mut()
        .object_set_prototype(array, Some(intrinsics.array_prototype()))?;
    }
    let length_key = alloc_key(scope, "length")?;
    scope.define_property(
      array,
      length_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Number(0.0),
          writable: true,
        },
      },
    )?;
    Ok(array)
  };

  let Some(document_obj) = mutation_observer_document_from_obj(scope, observer_obj)? else {
    let empty = alloc_empty_array(vm, scope)?;
    return Ok(Value::Object(empty));
  };

  let Some(dom) = dom_from_vm_host_mut(host) else {
    let empty = alloc_mutation_records_array(vm, scope, document_obj, None, &[])?;
    return Ok(Value::Object(empty));
  };
  let records = dom.mutation_observer_take_records(observer_id);
  let array = alloc_mutation_records_array(vm, scope, document_obj, Some(dom), &records)?;
  Ok(Value::Object(array))
}

fn mutation_observer_notify_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let document_obj = match slots
    .get(MUTATION_OBSERVER_NOTIFY_DOCUMENT_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Object(obj) => obj,
    _ => return Ok(Value::Undefined),
  };

  let deliveries = {
    let Some(dom) = dom_from_vm_host_mut(host) else {
      return Ok(Value::Undefined);
    };
    dom.mutation_observer_take_deliveries()
  };
  if deliveries.is_empty() {
    return Ok(Value::Undefined);
  }

  let registry = mutation_observer_registry_from_document(scope, document_obj)?;
  scope.push_root(Value::Object(registry))?;

  for (observer_id, records) in deliveries {
    if records.is_empty() {
      continue;
    }

    let key = alloc_key(scope, &observer_id.to_string())?;
    let observer_value = scope
      .heap()
      .object_get_own_data_property_value(registry, &key)?
      .unwrap_or(Value::Undefined);
    let Value::Object(observer_obj) = observer_value else {
      continue;
    };

    let callback_key = alloc_key(scope, MUTATION_OBSERVER_CALLBACK_KEY)?;
    let callback = scope
      .heap()
      .object_get_own_data_property_value(observer_obj, &callback_key)?
      .unwrap_or(Value::Undefined);
    if !matches!(callback, Value::Object(_)) || !scope.heap().is_callable(callback)? {
      continue;
    }

    let records_array = {
      let dom_for_wrappers = dom_from_vm_host(host);
      alloc_mutation_records_array(vm, scope, document_obj, dom_for_wrappers, &records)?
    };
    let args = [Value::Object(records_array), Value::Object(observer_obj)];
    // Per web platform behavior, exceptions from mutation observer callbacks should not abort the
    // checkpoint.
    let _ = vm.call_with_host_and_hooks(
      host,
      scope,
      hooks,
      callback,
      Value::Object(observer_obj),
      &args,
    );
  }

  Ok(Value::Undefined)
}

fn event_target_default_this_from_callee(
  scope: &Scope<'_>,
  callee: GcObject,
) -> Result<Option<GcObject>, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  Ok(
    match slots
      .get(EVENT_TARGET_DEFAULT_THIS_SLOT)
      .copied()
      .unwrap_or(Value::Undefined)
    {
      Value::Object(obj) => Some(obj),
      _ => None,
    },
  )
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

fn event_target_abort_cleanup_call_id_from_callee(
  scope: &Scope<'_>,
  callee: GcObject,
) -> Result<vm_js::NativeFunctionId, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  match slots
    .get(EVENT_TARGET_CONTEXT_ABORT_CLEANUP_CALL_ID_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Number(n) if n.is_finite() && n >= 0.0 => Ok(vm_js::NativeFunctionId(n as u32)),
    _ => Err(VmError::InvariantViolation(
      "EventTarget method missing required abort cleanup call id slot",
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
  target_id: web_events::EventTargetId,
}

fn resolve_dom_event_target(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  target_obj: GcObject,
) -> Result<(ResolvedDomEventTarget, NonNull<dom2::Document>), VmError> {
  let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
    return Err(VmError::InvariantViolation(
      "WindowRealm is missing required VM user data",
    ));
  };

  let Some(window_obj) = data.window_obj else {
    return Err(VmError::InvariantViolation(
      "window object missing from WindowRealmUserData",
    ));
  };

  let Some(document_obj) = data.document_obj else {
    return Err(VmError::InvariantViolation(
      "document object missing from WindowRealmUserData",
    ));
  };

  // Resolve the active DOM for this call turn. When there is no embedder DOM (dummy `VmHost`), fall
  // back to the realm-owned document so `window`/`document` events work in minimal realms.
  let host_dom_ptr = dom_ptr_for_event_registry(host);
  let (dom_ptr, has_host_dom) = if let Some(dom_ptr) = host_dom_ptr {
    (dom_ptr, true)
  } else {
    (NonNull::from(&mut data.events_dom_fallback), false)
  };

  let target_id = if target_obj == window_obj {
    web_events::EventTargetId::Window
  } else if target_obj == document_obj {
    web_events::EventTargetId::Document
  } else {
    // Node-backed targets are only supported when the call runs with a real host DOM.
    if !has_host_dom {
      return Err(VmError::TypeError("Illegal invocation"));
    }
    let platform = data.dom_platform.as_mut().ok_or_else(|| {
      VmError::InvariantViolation("WindowRealm is missing required DomPlatform for events")
    })?;
    platform.event_target_id_for_value(scope.heap(), Value::Object(target_obj))?
  };

  Ok((
    ResolvedDomEventTarget {
      window_obj,
      document_obj,
      target_id,
    },
    dom_ptr,
  ))
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

fn is_branded_abort_signal(scope: &mut Scope<'_>, obj: GcObject) -> Result<bool, VmError> {
  let key = alloc_key(scope, ABORT_SIGNAL_BRAND_KEY)?;
  Ok(matches!(
    scope.heap().object_get_own_data_property_value(obj, &key)?,
    Some(Value::Bool(true))
  ))
}

fn abort_signal_is_aborted(scope: &mut Scope<'_>, obj: GcObject) -> Result<bool, VmError> {
  let aborted_key = alloc_key(scope, "aborted")?;
  Ok(matches!(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &aborted_key)?,
    Some(Value::Bool(true))
  ))
}

fn resolve_event_target(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  callee: GcObject,
  target_obj: GcObject,
) -> Result<ResolvedEventTarget, VmError> {
  let (resolved_dom, dom_ptr) = match resolve_dom_event_target(vm, scope, host, target_obj) {
    Ok(ok) => ok,
    Err(err) => {
      // Non-DOM EventTarget objects (e.g. `AbortSignal`, `new EventTarget()`).
      if !is_branded_event_target(scope, target_obj)? {
        return Err(err);
      }

      let window_obj = event_target_context_global_from_callee(scope, callee)?;
      let (document_obj, dom_ptr) = {
        let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
          return Err(VmError::InvariantViolation(
            "missing WindowRealmUserData for event target resolution",
          ));
        };
        if data.window_obj != Some(window_obj) {
          return Err(VmError::InvariantViolation(
            "EventTarget global mismatch for WindowRealm user data",
          ));
        }
        let Some(document_obj) = data.document_obj else {
          return Err(VmError::InvariantViolation(
            "document object missing from WindowRealmUserData",
          ));
        };
        let dom_ptr = dom_ptr_for_event_registry(host)
          .unwrap_or_else(|| NonNull::from(&mut data.events_dom_fallback));
        (document_obj, dom_ptr)
      };

      return Ok(ResolvedEventTarget {
        // Root listener callbacks on the target object itself.
        //
        // When dispatching through an explicit parent chain (`new EventTarget(parent)`), the vm-js
        // event invoker uses the registry's weak mapping from `EventTargetId::Opaque` IDs back to
        // JS objects to locate per-target callback roots.
        listener_roots_owner: target_obj,
        resolved: ResolvedDomEventTarget {
          window_obj,
          document_obj,
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
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
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
      let v = vm.get_with_host_and_hooks(host, scope, hooks, obj, capture_key)?;
      opts.capture = scope.heap().to_boolean(v)?;

      let once_key = alloc_key(scope, "once")?;
      let v = vm.get_with_host_and_hooks(host, scope, hooks, obj, once_key)?;
      opts.once = scope.heap().to_boolean(v)?;

      let passive_key = alloc_key(scope, "passive")?;
      let v = vm.get_with_host_and_hooks(host, scope, hooks, obj, passive_key)?;
      opts.passive = scope.heap().to_boolean(v)?;

      Ok(opts)
    }
    _ => Ok(opts),
  }
}

fn parse_event_listener_capture(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<bool, VmError> {
  Ok(match value {
    Value::Bool(b) => b,
    Value::Object(obj) => {
      let capture_key = alloc_key(scope, "capture")?;
      let v = vm.get_with_host_and_hooks(host, scope, hooks, obj, capture_key)?;
      scope.heap().to_boolean(v)?
    }
    _ => false,
  })
}

fn get_or_create_event_listener_roots(
  scope: &mut Scope<'_>,
  owner_obj: GcObject,
) -> Result<GcObject, VmError> {
  let key = alloc_key(scope, EVENT_LISTENER_ROOTS_KEY)?;
  if let Some(Value::Object(obj)) = scope
    .heap()
    .object_get_own_data_property_value(owner_obj, &key)?
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
  let Some(Value::Object(roots)) = scope
    .heap()
    .object_get_own_data_property_value(roots_owner_obj, &roots_key)?
  else {
    return Ok(());
  };
  let listener_key = listener_id_property_key(scope, listener_id)?;
  let _ = scope.ordinary_delete(roots, listener_key)?;
  Ok(())
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
pub(crate) struct WindowRealmDomEventListenerInvoker<Host: WindowRealmHost + 'static> {
  /// Pointer to the owning executor's `Option<WindowRealm>` slot.
  ///
  /// This is used so Rust-driven DOM event dispatch can invoke JS listeners without requiring the
  /// caller to thread `&mut WindowRealm` through `web_events::dispatch_event`.
  realm: *mut Option<WindowRealm>,
  /// Pointer to the owning executor's current `VmHost` context.
  ///
  /// When the JS callback runs we must pass the real embedder `VmHost` so native functions invoked
  /// inside listeners (e.g. `fetch`, WebIDL ops, embedder test hooks) can access per-document host
  /// state.
  vm_host: *mut Option<NonNull<dyn VmHost>>,
  /// Pointer to the owning executor's `Option<WebIdlBindingsHost>` slot.
  ///
  /// This is used by host-driven DOM event dispatch paths that only have access to a `VmHost`
  /// context and therefore cannot populate the WebIDL slot via `WindowRealmHost`.
  webidl_bindings_host: *mut Option<NonNull<dyn WebIdlBindingsHost>>,
  /// Optional active event loop for this dispatch.
  ///
  /// Host-driven DOM event dispatch (`BrowserTabHost::dispatch_dom_event_in_event_loop`) needs to
  /// thread a `&mut EventLoop<Host>` through `web_events::dispatch_event`, which only provides
  /// `&mut dyn EventListenerInvoker`. The embedding installs the event loop pointer via
  /// [`WindowRealmDomEventListenerInvoker::with_event_loop`], and `invoke` forwards it into the
  /// `VmJsEventLoopHooks` payload so Web APIs like `queueMicrotask` can enqueue work.
  event_loop: Option<NonNull<EventLoop<Host>>>,
  _marker: PhantomData<fn() -> Host>,
}

impl<Host: WindowRealmHost + 'static> WindowRealmDomEventListenerInvoker<Host> {
  pub(crate) fn new(
    realm: *mut Option<WindowRealm>,
    vm_host: *mut Option<NonNull<dyn VmHost>>,
    webidl_bindings_host: *mut Option<NonNull<dyn WebIdlBindingsHost>>,
  ) -> Self {
    Self {
      realm,
      vm_host,
      webidl_bindings_host,
      event_loop: None,
      _marker: PhantomData,
    }
  }

  pub(crate) fn with_event_loop<R>(
    &mut self,
    event_loop: &mut EventLoop<Host>,
    f: impl FnOnce(&mut Self) -> R,
  ) -> R {
    struct EventLoopSwapGuard<'a, Host: WindowRealmHost + 'static> {
      invoker: &'a mut WindowRealmDomEventListenerInvoker<Host>,
      prev: Option<NonNull<EventLoop<Host>>>,
    }

    impl<Host: WindowRealmHost + 'static> Drop for EventLoopSwapGuard<'_, Host> {
      fn drop(&mut self) {
        self.invoker.event_loop = self.prev;
      }
    }

    let prev = self.event_loop;
    self.event_loop = Some(NonNull::from(event_loop));
    let mut guard = EventLoopSwapGuard {
      invoker: self,
      prev,
    };
    f(&mut *guard.invoker)
  }

  fn current_event_loop_mut(&mut self) -> Option<&mut EventLoop<Host>> {
    let mut ptr = self.event_loop?;
    Some(unsafe { ptr.as_mut() })
  }

  pub(crate) fn invoke_event_handler_property(
    &mut self,
    target: web_events::EventTargetId,
    event: &mut web_events::Event,
  ) -> std::result::Result<(), web_events::DomError> {
    let target = target.normalize();

    // SAFETY: `BrowserTabHost` stores the returned invoker alongside the owning executor, so the
    // pointer remains valid for the lifetime of the host. Dispatch is single-threaded and
    // non-reentrant with respect to other `WindowRealm` borrows.
    let Some(realm) = unsafe { &mut *self.realm }.as_mut() else {
      return Ok(());
    };

    let Some(mut host_ptr) = (unsafe { *self.vm_host }) else {
      return Ok(());
    };
    // SAFETY: The embedding stores a stable heap-allocated host context (e.g. `BrowserDocumentDom2`)
    // for the lifetime of the `WindowRealm` and updates the pointer on navigations.
    let host_ctx: &mut dyn VmHost = unsafe { host_ptr.as_mut() };

    let webidl_bindings_host: Option<&mut dyn WebIdlBindingsHost> =
      unsafe { *self.webidl_bindings_host }.map(|mut host| unsafe { host.as_mut() });
    let mut host_hooks = VmJsEventLoopHooks::<Host>::new_with_vm_host_and_window_realm(
      host_ctx,
      realm,
      webidl_bindings_host,
    );
    if let Some(event_loop) = self.current_event_loop_mut() {
      host_hooks.set_event_loop(event_loop);
    }

    // Host-driven dispatch is a "new turn" of JS execution: clear any prior termination state and
    // install the latest per-run budgets from `JsExecutionOptions` so hostile handlers cannot hang
    // the host.
    realm.reset_interrupt();
    let budget = realm.vm_budget_now();

    let realm_id = realm.realm_id;
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut vm = vm.push_budget(budget);
    vm.tick()
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
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

    let dom_for_wrappers = dom_ptr_for_event_registry(host_ctx).map(|ptr| unsafe { ptr.as_ref() });
    let target_value = Self::js_value_for_target(
      &mut vm,
      &mut scope,
      window_obj,
      document_obj,
      dom_for_wrappers,
      Some(target),
    )
    .map_err(|e| web_events::DomError::new(e.to_string()))?;
    let Value::Object(target_obj) = target_value else {
      return Ok(());
    };

    // Look up `on{type}` on the target object. This intentionally only checks own data properties:
    // we do not yet have IDL EventHandler attribute plumbing (which would involve prototype
    // accessors + stable callback storage on the underlying DOM node).
    let handler_name = format!("on{}", event.type_);
    scope
      .push_root(Value::Object(target_obj))
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
    let handler_key =
      alloc_key(&mut scope, &handler_name).map_err(|e| web_events::DomError::new(e.to_string()))?;
    let Some(handler) = scope
      .heap()
      .object_get_own_data_property_value(target_obj, &handler_key)
      .map_err(|e| web_events::DomError::new(e.to_string()))?
    else {
      return Ok(());
    };
    if !scope
      .heap()
      .is_callable(handler)
      .map_err(|e| web_events::DomError::new(e.to_string()))?
    {
      return Ok(());
    }

    // Expose `currentTarget`/`eventPhase` while the handler runs.
    struct EventStateGuard<'a> {
      event: &'a mut web_events::Event,
      prev_target: Option<web_events::EventTargetId>,
      prev_current_target: Option<web_events::EventTargetId>,
      prev_phase: web_events::EventPhase,
    }

    impl Drop for EventStateGuard<'_> {
      fn drop(&mut self) {
        self.event.target = self.prev_target;
        self.event.current_target = self.prev_current_target;
        self.event.event_phase = self.prev_phase;
      }
    }

    let mut state_guard = EventStateGuard {
      prev_target: event.target,
      prev_current_target: event.current_target,
      prev_phase: event.event_phase,
      event,
    };
    state_guard.event.target = Some(target);
    state_guard.event.current_target = Some(target);
    state_guard.event.event_phase = web_events::EventPhase::AtTarget;

    let event_obj = Self::alloc_js_event_object(&mut scope, document_obj, state_guard.event)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
    scope
      .push_root(Value::Object(event_obj))
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
    Self::sync_event_object(
      &mut vm,
      &mut scope,
      window_obj,
      document_obj,
      dom_for_wrappers,
      event_obj,
      state_guard.event,
    )
    .map_err(|e| web_events::DomError::new(e.to_string()))?;

    let event_id = NEXT_ACTIVE_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    let _active_guard = push_active_event_for_host(host_ctx, event_id, state_guard.event);
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

    // Invoke callback, swallowing exceptions to match web platform behavior.
    let call_result =
      vm.call_with_host_and_hooks(host_ctx, &mut scope, &mut host_hooks, handler, target_value, &[Value::Object(event_obj)]);
    match call_result {
      Ok(ret) => {
        // HTML EventHandler semantics: returning `false` cancels the event.
        if matches!(ret, Value::Bool(false)) {
          state_guard.event.prevent_default();
        }
      }
      Err(err) => {
        // Termination (out of fuel, interrupted, deadline exceeded) is not a "normal" exception: it
        // is a safety mechanism enforced by the host, so it must propagate to the embedding.
        if matches!(err, VmError::Termination(_)) {
          return Err(web_events::DomError::new(err.to_string()));
        }
      }
    }

    // Mirror JS-visible flags back onto the Rust event in case Event.prototype methods were invoked
    // with a host that does not support `ActiveEventStack`.
    sync_rust_event_from_js_event_object(&mut scope, event_obj, state_guard.event)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;

    if let Some(err) = host_hooks.finish(scope.heap_mut()) {
      return Err(web_events::DomError::new(err.to_string()));
    }

    Ok(())
  }

  fn js_value_for_target(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    window_obj: GcObject,
    document_obj: GcObject,
    dom: Option<&dom2::Document>,
    target: Option<web_events::EventTargetId>,
  ) -> Result<Value, VmError> {
    match target {
      None => Ok(Value::Null),
      Some(web_events::EventTargetId::Window) => Ok(Value::Object(window_obj)),
      Some(web_events::EventTargetId::Document) => Ok(Value::Object(document_obj)),
      Some(web_events::EventTargetId::Node(node_id)) => {
        get_or_create_node_wrapper(vm, scope, document_obj, dom, node_id)
      }
      Some(web_events::EventTargetId::Opaque(_)) => Ok(Value::Null),
    }
  }

  fn alloc_js_event_object(
    scope: &mut Scope<'_>,
    document_obj: GcObject,
    event: &web_events::Event,
  ) -> Result<GcObject, VmError> {
    let proto_key_name = if event.storage.is_some() {
      STORAGE_EVENT_PROTOTYPE_KEY
    } else if event.detail.is_some() {
      CUSTOM_EVENT_PROTOTYPE_KEY
    } else if event.type_ == "storage" {
      STORAGE_EVENT_PROTOTYPE_KEY
    } else {
      EVENT_PROTOTYPE_KEY
    };
    let proto_key = alloc_key(scope, proto_key_name)?;
    let mut proto = scope
      .heap()
      .object_get_own_data_property_value(document_obj, &proto_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      });

    // `StorageEvent` is installed separately from this file (see the StorageEvent constructor task).
    // To avoid making host-driven storage event dispatch depend on init ordering, lazily resolve
    // `StorageEvent.prototype` from the window global if the document doesn't have it cached yet.
    if proto.is_none() && event.storage.is_some() {
      if let Some(window_obj) = document_window_from_document(scope, document_obj)? {
        scope.push_root(Value::Object(window_obj))?;
        let ctor_key = alloc_key(scope, "StorageEvent")?;
        let ctor = scope
          .heap()
          .object_get_own_data_property_value(window_obj, &ctor_key)?;
        if let Some(Value::Object(ctor_obj)) = ctor {
          scope.push_root(Value::Object(ctor_obj))?;
          let prototype_key = alloc_key(scope, "prototype")?;
          proto = scope
            .heap()
            .object_get_own_data_property_value(ctor_obj, &prototype_key)?
            .and_then(|v| match v {
              Value::Object(obj) => Some(obj),
              _ => None,
            });
          if let Some(proto_obj) = proto {
            scope.push_root(Value::Object(proto_obj))?;
            scope.define_property(document_obj, proto_key, data_desc(Value::Object(proto_obj)))?;
          }
        }
      }
    }

    // If the realm doesn't provide `StorageEvent` yet, fall back to `Event.prototype` so host-driven
    // storage events can still be observed. (The StorageEvent constructor task wires up the real
    // prototype chain; this fallback keeps behavior usable until then.)
    if proto.is_none() && event.storage.is_some() {
      let event_proto_key = alloc_key(scope, EVENT_PROTOTYPE_KEY)?;
      proto = scope
        .heap()
        .object_get_own_data_property_value(document_obj, &event_proto_key)?
        .and_then(|v| match v {
          Value::Object(obj) => Some(obj),
          _ => None,
        });
    }

    let proto = match proto {
      Some(proto) => proto,
      None => {
        return Err(VmError::InvariantViolation(match proto_key_name {
          STORAGE_EVENT_PROTOTYPE_KEY => "document is missing required StorageEvent prototype",
          CUSTOM_EVENT_PROTOTYPE_KEY => "document is missing required CustomEvent prototype",
          _ => "document is missing required Event prototype",
        }))
      }
    };

    let event_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(event_obj))?;
    scope
      .heap_mut()
      .object_set_prototype(event_obj, Some(proto))?;

    // Base event fields (immutable for the lifetime of this dispatch).
    let type_key = alloc_key(scope, "type")?;
    let type_s = scope.alloc_string(&event.type_)?;
    scope.push_root(Value::String(type_s))?;
    scope.define_property(event_obj, type_key, data_desc(Value::String(type_s)))?;

    let bubbles_key = alloc_key(scope, "bubbles")?;
    scope.define_property(
      event_obj,
      bubbles_key,
      data_desc(Value::Bool(event.bubbles)),
    )?;

    let cancelable_key = alloc_key(scope, "cancelable")?;
    scope.define_property(
      event_obj,
      cancelable_key,
      data_desc(Value::Bool(event.cancelable)),
    )?;

    let composed_key = alloc_key(scope, "composed")?;
    scope.define_property(
      event_obj,
      composed_key,
      data_desc(Value::Bool(event.composed)),
    )?;

    if let Some(detail) = event.detail {
      let detail_key = alloc_key(scope, "detail")?;
      scope.define_property(event_obj, detail_key, data_desc(detail))?;
    } else if event.type_ == "storage" {
      // StorageEvent fields (not yet modelled in Rust `web_events::Event`).
      let key_key = alloc_key(scope, "key")?;
      scope.define_property(event_obj, key_key, read_only_data_desc(Value::Null))?;
      let old_value_key = alloc_key(scope, "oldValue")?;
      scope.define_property(event_obj, old_value_key, read_only_data_desc(Value::Null))?;
      let new_value_key = alloc_key(scope, "newValue")?;
      scope.define_property(event_obj, new_value_key, read_only_data_desc(Value::Null))?;
      let url_key = alloc_key(scope, "url")?;
      let empty = scope.alloc_string("")?;
      scope.define_property(event_obj, url_key, read_only_data_desc(Value::String(empty)))?;
      let storage_area_key = alloc_key(scope, "storageArea")?;
      scope.define_property(
        event_obj,
        storage_area_key,
        read_only_data_desc(Value::Null),
      )?;
    }

    if let Some(storage) = event.storage.as_ref() {
      let key_key = alloc_key(scope, "key")?;
      let key_v = match storage.key.as_deref() {
        Some(s) => {
          let s = scope.alloc_string(s)?;
          scope.push_root(Value::String(s))?;
          Value::String(s)
        }
        None => Value::Null,
      };
      scope.define_property(event_obj, key_key, data_desc(key_v))?;

      let old_value_key = alloc_key(scope, "oldValue")?;
      let old_value_v = match storage.old_value.as_deref() {
        Some(s) => {
          let s = scope.alloc_string(s)?;
          scope.push_root(Value::String(s))?;
          Value::String(s)
        }
        None => Value::Null,
      };
      scope.define_property(event_obj, old_value_key, data_desc(old_value_v))?;

      let new_value_key = alloc_key(scope, "newValue")?;
      let new_value_v = match storage.new_value.as_deref() {
        Some(s) => {
          let s = scope.alloc_string(s)?;
          scope.push_root(Value::String(s))?;
          Value::String(s)
        }
        None => Value::Null,
      };
      scope.define_property(event_obj, new_value_key, data_desc(new_value_v))?;

      let url_key = alloc_key(scope, "url")?;
      let url_s = scope.alloc_string(&storage.url)?;
      scope.push_root(Value::String(url_s))?;
      scope.define_property(event_obj, url_key, data_desc(Value::String(url_s)))?;

      let storage_area_key = alloc_key(scope, "storageArea")?;
      let storage_area_v = (|| {
        let window_obj = document_window_from_document(scope, document_obj)?;
        let Some(window_obj) = window_obj else {
          return Ok(Value::Null);
        };
        scope.push_root(Value::Object(window_obj))?;
        let storage_key = match storage.storage_kind {
          web_events::StorageKind::Local => alloc_key(scope, "localStorage")?,
          web_events::StorageKind::Session => alloc_key(scope, "sessionStorage")?,
        };
        Ok(
          scope
            .heap()
            .object_get_own_data_property_value(window_obj, &storage_key)?
            .unwrap_or(Value::Null),
        )
      })()?;
      scope.define_property(event_obj, storage_area_key, data_desc(storage_area_v))?;
    }

    Ok(event_obj)
  }

  fn sync_event_object(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    window_obj: GcObject,
    document_obj: GcObject,
    dom: Option<&dom2::Document>,
    event_obj: GcObject,
    event: &web_events::Event,
  ) -> Result<(), VmError> {
    scope.push_root(Value::Object(event_obj))?;

    let target_key = alloc_key(scope, "target")?;
    let target_v =
      Self::js_value_for_target(vm, scope, window_obj, document_obj, dom, event.target)?;
    scope.define_property(event_obj, target_key, data_desc(target_v))?;

    let current_target_key = alloc_key(scope, "currentTarget")?;
    let current_target_v = Self::js_value_for_target(
      vm,
      scope,
      window_obj,
      document_obj,
      dom,
      event.current_target,
    )?;
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

    let immediate_key = alloc_key(scope, EVENT_IMMEDIATE_STOP_KEY)?;
    scope.define_property(
      event_obj,
      immediate_key,
      data_desc(Value::Bool(event.immediate_propagation_stopped)),
    )?;

    Ok(())
  }
}

fn sync_rust_event_from_js_event_object(
  scope: &mut Scope<'_>,
  event_obj: GcObject,
  event: &mut web_events::Event,
) -> Result<(), VmError> {
  let cancel_bubble_key = alloc_key(scope, "cancelBubble")?;
  let cancel_bubble = scope
    .heap()
    .object_get_own_data_property_value(event_obj, &cancel_bubble_key)?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?
    .unwrap_or(false);
  if cancel_bubble {
    event.stop_propagation();
  }

  let immediate_key = alloc_key(scope, EVENT_IMMEDIATE_STOP_KEY)?;
  let immediate = scope
    .heap()
    .object_get_own_data_property_value(event_obj, &immediate_key)?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?
    .unwrap_or(false);
  if immediate {
    event.stop_immediate_propagation();
  }

  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  let default_prevented = scope
    .heap()
    .object_get_own_data_property_value(event_obj, &default_prevented_key)?
    .map(|v| scope.heap().to_boolean(v))
    .transpose()?
    .unwrap_or(false);
  if default_prevented {
    event.default_prevented = true;
  }

  Ok(())
}

impl<Host: WindowRealmHost + 'static> web_events::EventListenerInvoker
  for WindowRealmDomEventListenerInvoker<Host>
{
  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    Some(self)
  }

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

    // Invoke callback, swallowing exceptions to match web platform behavior.
    let Some(mut host_ptr) = (unsafe { *self.vm_host }) else {
      // No host context available; treat as no-op.
      return Ok(());
    };
    // SAFETY: The embedding stores a stable heap-allocated host context (e.g. `BrowserDocumentDom2`)
    // for the lifetime of the `WindowRealm` and updates the pointer on navigations.
    let host_ctx: &mut dyn VmHost = unsafe { host_ptr.as_mut() };

    // Route Promise jobs (and other hooks) through the host event loop microtask queue.
    //
    // Use the borrow-split constructor so hooks can discard pending jobs safely if enqueuing fails
    // or the realm is torn down with outstanding jobs.
    let webidl_bindings_host: Option<&mut dyn WebIdlBindingsHost> =
      unsafe { *self.webidl_bindings_host }.map(|mut host| unsafe { host.as_mut() });
    let mut host_hooks = VmJsEventLoopHooks::<Host>::new_with_vm_host_and_window_realm(
      host_ctx,
      realm,
      webidl_bindings_host,
    );
    if let Some(event_loop) = self.current_event_loop_mut() {
      host_hooks.set_event_loop(event_loop);
    }

    // Host-driven dispatch is a "new turn" of JS execution: clear any prior termination state and
    // install the latest per-run budgets from `JsExecutionOptions` so hostile listeners cannot hang
    // the host.
    realm.reset_interrupt();
    let budget = realm.vm_budget_now();

    let realm_id = realm.realm_id;
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    let mut vm = vm.push_budget(budget);
    // Ensure immediate termination when no budget remains (deadline exceeded, interrupted, etc).
    vm.tick()
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
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

    // Resolve the embedder `VmHost` (if available) so we can map DOM event targets back into full
    // JS wrappers (Element/Text/etc) when updating `event.target/currentTarget`.
    let Some(mut host_ptr) = (unsafe { *self.vm_host }) else {
      // No host context available; treat as no-op.
      return Ok(());
    };
    // SAFETY: The embedding stores a stable heap-allocated host context (e.g. `BrowserDocumentDom2`)
    // for the lifetime of the `WindowRealm` and updates the pointer on navigations.
    let host_ctx: &mut dyn VmHost = unsafe { host_ptr.as_mut() };
    let dom_for_wrappers = dom_ptr_for_event_registry(host_ctx).map(|ptr| unsafe { ptr.as_ref() });

    Self::sync_event_object(
      &mut vm,
      &mut scope,
      window_obj,
      document_obj,
      dom_for_wrappers,
      event_obj,
      event,
    )
    .map_err(|e| web_events::DomError::new(e.to_string()))?;

    let current_target = match event.current_target {
      Some(t) => Self::js_value_for_target(
        &mut vm,
        &mut scope,
        window_obj,
        document_obj,
        dom_for_wrappers,
        Some(t),
      )
      .map_err(|e| web_events::DomError::new(e.to_string()))?,
      None => Value::Undefined,
    };

    let event_id = NEXT_ACTIVE_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    let _active_guard = push_active_event_for_host(host_ctx, event_id, event);
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
    let call_result: Result<(), VmError> = (|| {
      let event_value = Value::Object(event_obj);
      if scope.heap().is_callable(callback)? {
        vm.call_with_host_and_hooks(
          host_ctx,
          &mut scope,
          &mut host_hooks,
          callback,
          current_target,
          &[event_value],
        )?;
        Ok(())
      } else if let Value::Object(callback_obj) = callback {
        let handle_event_key = alloc_key(&mut scope, "handleEvent")?;
        let handle_event = vm.get_with_host_and_hooks(
          host_ctx,
          &mut scope,
          &mut host_hooks,
          callback_obj,
          handle_event_key,
        )?;
        if !scope.heap().is_callable(handle_event)? {
          return Err(VmError::TypeError(
            "EventTarget listener callback has no callable handleEvent",
          ));
        }
        vm.call_with_host_and_hooks(
          host_ctx,
          &mut scope,
          &mut host_hooks,
          handle_event,
          callback,
          &[event_value],
        )?;
        Ok(())
      } else {
        Err(VmError::TypeError(
          "EventTarget listener is not callable and not an object",
        ))
      }
    })();

    // If the JS `Event.prototype` methods were invoked with a host context that does not support
    // `ActiveEventStack`, they still update the JS-visible flags (`cancelBubble`, `defaultPrevented`,
    // etc). Mirror those back onto the shared Rust `Event` so the DOM dispatch algorithm can observe
    // propagation control and cancellation.
    sync_rust_event_from_js_event_object(&mut scope, event_obj, event)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;

    if let Some(err) = host_hooks.finish(scope.heap_mut()) {
      return Err(web_events::DomError::new(err.to_string()));
    }

    if let Err(err) = call_result {
      // Per web platform behavior, exceptions from event listeners should not abort dispatch.
      //
      // Termination (out of fuel, interrupted, deadline exceeded) is not a "normal" exception: it
      // is a safety mechanism enforced by the host, so it must propagate to the embedding.
      if matches!(err, VmError::Termination(_)) {
        return Err(web_events::DomError::new(err.to_string()));
      }
    }

    // Best-effort cleanup: remove callback roots for `{ once: true }` listeners.
    if let Some(registry) = (&*vm).user_data::<WindowRealmUserData>().map(|data| {
      if let Some(dom) = dom_from_vm_host(host_ctx) {
        dom.events()
      } else {
        data.events_dom_fallback.events()
      }
    }) {
      let _ = remove_listener_root_if_unused(&mut scope, document_obj, registry, listener_id, None);
    }

    Ok(())
  }
}

struct VmJsDomEventInvoker<'a, 'hooks> {
  vm: *mut Vm,
  scope: *mut Scope<'a>,
  vm_host: *mut dyn VmHost,
  hooks: *mut (dyn VmHostHooks + 'hooks),
  window_obj: GcObject,
  document_obj: GcObject,
  event_obj: GcObject,
  dom_ptr: NonNull<dom2::Document>,
  /// Callback roots stored on the realm's document object (used for DOM-backed targets).
  document_listener_roots: GcObject,
  opaque_target_obj: Option<GcObject>,
  registry: *const web_events::EventListenerRegistry,
}

impl<'a, 'hooks> VmJsDomEventInvoker<'a, 'hooks> {
  fn opaque_target_obj_for_id(&mut self, id: u64) -> Option<GcObject> {
    let scope = unsafe { &mut *self.scope };
    let registry = unsafe { &*self.registry };
    registry.opaque_target_object(scope.heap(), id).or_else(|| {
      self
        .opaque_target_obj
        .filter(|obj| gc_object_id(*obj) == id)
    })
  }

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
        let dom = unsafe { self.dom_ptr.as_ref() };
        get_or_create_node_wrapper(vm, scope, self.document_obj, Some(dom), node_id)
      }
      Some(web_events::EventTargetId::Opaque(id)) => Ok(match self.opaque_target_obj_for_id(id) {
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
    scope.define_property(
      self.event_obj,
      current_target_key,
      data_desc(current_target_v),
    )?;

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

    let immediate_key = alloc_key(scope, EVENT_IMMEDIATE_STOP_KEY)?;
    scope.define_property(
      self.event_obj,
      immediate_key,
      data_desc(Value::Bool(event.immediate_propagation_stopped)),
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
    let vm_host = unsafe { &mut *self.vm_host };
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
      vm_host,
      scope,
      hooks,
      func,
      Value::Object(console_obj),
      &[Value::String(msg_s)],
    );
  }
}

impl web_events::EventListenerInvoker for VmJsDomEventInvoker<'_, '_> {
  fn invoke(
    &mut self,
    listener_id: web_events::ListenerId,
    event: &mut web_events::Event,
  ) -> std::result::Result<(), web_events::DomError> {
    let scope = unsafe { &mut *self.scope };
    let vm = unsafe { &mut *self.vm };
    let vm_host = unsafe { &mut *self.vm_host };
    let hooks = unsafe { &mut *self.hooks };
    let registry = unsafe { &*self.registry };

    // Look up the registered callback function/object. If it is missing, treat it as a no-op.
    //
    // DOM-backed targets root callbacks on the realm's `document` object.
    // Non-DOM `EventTargetId::Opaque` targets root callbacks on the target object itself.
    let (listener_roots_owner, listener_roots, target_for_owner) = match event.current_target {
      Some(web_events::EventTargetId::Opaque(id)) => {
        let Some(owner_obj) = self.opaque_target_obj_for_id(id) else {
          return Ok(());
        };
        let roots_key = alloc_key(scope, EVENT_LISTENER_ROOTS_KEY)
          .map_err(|e| web_events::DomError::new(e.to_string()))?;
        let Some(Value::Object(roots)) = scope
          .heap()
          .object_get_own_data_property_value(owner_obj, &roots_key)
          .map_err(|e| web_events::DomError::new(e.to_string()))?
        else {
          return Ok(());
        };
        (
          owner_obj,
          roots,
          Some(web_events::EventTargetId::Opaque(id)),
        )
      }
      _ => (self.document_obj, self.document_listener_roots, None),
    };

    let listener_key = listener_id_property_key(scope, listener_id)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;
    let Some(callback) = scope
      .heap()
      .object_get_own_data_property_value(listener_roots, &listener_key)
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
    let _active_guard = push_active_event_for_host(vm_host, event_id, event);

    let event_id_key =
      alloc_key(scope, EVENT_ID_KEY).map_err(|e| web_events::DomError::new(e.to_string()))?;
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
      Some(web_events::EventTargetId::Opaque(_)) => Value::Object(listener_roots_owner),
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
        vm.call_with_host_and_hooks(
          vm_host,
          scope,
          hooks,
          callback,
          current_target,
          &[event_value],
        )?;
        Ok(())
      } else if let Value::Object(callback_obj) = callback {
        let handle_event_key = alloc_key(scope, "handleEvent")?;
        let handle_event =
          vm.get_with_host_and_hooks(vm_host, scope, hooks, callback_obj, handle_event_key)?;
        if !scope.heap().is_callable(handle_event)? {
          return Err(VmError::TypeError(
            "EventTarget listener callback has no callable handleEvent",
          ));
        }
        vm.call_with_host_and_hooks(
          vm_host,
          scope,
          hooks,
          handle_event,
          callback,
          &[event_value],
        )?;
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

    // Ensure cancellation/propagation flags set via the JS `Event` instance are reflected onto the
    // shared Rust `Event` so the dispatcher can observe them even when no host-side
    // `ActiveEventStack` is installed.
    sync_rust_event_from_js_event_object(scope, self.event_obj, event)
      .map_err(|e| web_events::DomError::new(e.to_string()))?;

    // `dispatch_event` can remove listeners during dispatch (`{ once: true }`). Drop the callback
    // root if the listener ID is no longer referenced.
    if let Err(err) = remove_listener_root_if_unused(
      scope,
      listener_roots_owner,
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
  vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  event_obj: GcObject,
) -> Result<web_events::Event, VmError> {
  let type_key = alloc_key(scope, "type")?;
  let type_value = vm.get_with_host_and_hooks(vm_host, scope, hooks, event_obj, type_key)?;
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

const ABORT_SIGNAL_CLEANUP_TARGET_KIND_SLOT: usize = 0;
const ABORT_SIGNAL_CLEANUP_TARGET_ID_SLOT: usize = 1;
const ABORT_SIGNAL_CLEANUP_TYPE_SLOT: usize = 2;
const ABORT_SIGNAL_CLEANUP_LISTENER_ID_SLOT: usize = 3;
const ABORT_SIGNAL_CLEANUP_CAPTURE_SLOT: usize = 4;

fn abort_signal_listener_cleanup_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;

  let target_kind = match slots
    .get(ABORT_SIGNAL_CLEANUP_TARGET_KIND_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Number(n) if n.is_finite() && n >= 0.0 => n as u8,
    _ => {
      return Err(VmError::InvariantViolation(
        "AbortSignal cleanup missing target kind slot",
      ))
    }
  };

  let target_id = match target_kind {
    0 => web_events::EventTargetId::Window,
    1 => web_events::EventTargetId::Document,
    2 => match slots
      .get(ABORT_SIGNAL_CLEANUP_TARGET_ID_SLOT)
      .copied()
      .unwrap_or(Value::Undefined)
    {
      Value::Number(n) if n.is_finite() && n >= 0.0 => {
        web_events::EventTargetId::Node(dom2::NodeId::from_index(n as usize))
      }
      _ => {
        return Err(VmError::InvariantViolation(
          "AbortSignal cleanup missing node id slot",
        ))
      }
    },
    3 => match slots
      .get(ABORT_SIGNAL_CLEANUP_TARGET_ID_SLOT)
      .copied()
      .unwrap_or(Value::Undefined)
    {
      Value::String(s) => {
        let raw = scope.heap().get_string(s)?.to_utf8_lossy();
        let id = raw.parse::<u64>().map_err(|_| {
          VmError::InvariantViolation("AbortSignal cleanup has invalid opaque target id")
        })?;
        web_events::EventTargetId::Opaque(id)
      }
      _ => {
        return Err(VmError::InvariantViolation(
          "AbortSignal cleanup missing opaque target id slot",
        ))
      }
    },
    _ => {
      return Err(VmError::InvariantViolation(
        "AbortSignal cleanup has unknown target kind",
      ))
    }
  };

  let type_s = match slots
    .get(ABORT_SIGNAL_CLEANUP_TYPE_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "AbortSignal cleanup missing type slot",
      ))
    }
  };
  let type_name = scope.heap().get_string(type_s)?.to_utf8_lossy();

  let listener_id_s = match slots
    .get(ABORT_SIGNAL_CLEANUP_LISTENER_ID_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::String(s) => s,
    _ => {
      return Err(VmError::InvariantViolation(
        "AbortSignal cleanup missing listener id slot",
      ))
    }
  };
  let listener_raw = scope.heap().get_string(listener_id_s)?.to_utf8_lossy();
  let listener_id = web_events::ListenerId::new(
    listener_raw
      .parse::<u64>()
      .map_err(|_| VmError::InvariantViolation("AbortSignal cleanup has invalid listener id"))?,
  );

  let capture = match slots
    .get(ABORT_SIGNAL_CLEANUP_CAPTURE_SLOT)
    .copied()
    .unwrap_or(Value::Undefined)
  {
    Value::Bool(b) => b,
    _ => false,
  };

  let Some(data) = vm.user_data_mut::<WindowRealmUserData>() else {
    return Err(VmError::InvariantViolation(
      "AbortSignal cleanup missing WindowRealm user data",
    ));
  };
  let Some(document_obj) = data.document_obj else {
    return Err(VmError::InvariantViolation(
      "AbortSignal cleanup missing document object",
    ));
  };

  let mut dom_ptr = dom_ptr_for_event_registry(host)
    .unwrap_or_else(|| NonNull::from(&mut data.events_dom_fallback));

  // SAFETY: `dom_ptr` is derived from the current `VmHost` (or the realm's fallback document) and
  // is only used for the duration of this native call.
  let dom = unsafe { dom_ptr.as_mut() };
  let removed = dom
    .events_mut()
    .remove_event_listener(target_id, &type_name, listener_id, capture);
  if removed {
    remove_listener_root_if_unused(scope, document_obj, dom.events(), listener_id, None)?;
  }

  Ok(Value::Undefined)
}

fn event_target_add_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target_obj = event_target_resolve_this(scope, callee, this)?;
  let resolved = resolve_event_target(vm, scope, host, callee, target_obj)?;
  let ResolvedEventTarget {
    resolved,
    mut dom_ptr,
    listener_roots_owner,
    ..
  } = resolved;

  let type_arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let type_string = scope.heap_mut().to_string(type_arg)?;
  scope.push_root(Value::String(type_string))?;
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
  let options = parse_add_event_listener_options(vm, scope, host, hooks, options_value)?;
  let mut signal_obj: Option<GcObject> = None;
  if let Value::Object(options_obj) = options_value {
    let signal_key = alloc_key(scope, "signal")?;
    let signal_value = vm.get_with_host_and_hooks(host, scope, hooks, options_obj, signal_key)?;
    match signal_value {
      Value::Undefined | Value::Null => {}
      Value::Object(obj) => {
        if !is_branded_abort_signal(scope, obj)? {
          return Err(VmError::TypeError(
            "EventTarget.addEventListener: options.signal must be an AbortSignal",
          ));
        }
        if abort_signal_is_aborted(scope, obj)? {
          // Per spec, listeners added with an already-aborted signal are ignored.
          return Ok(Value::Undefined);
        }
        signal_obj = Some(obj);
      }
      _ => {
        return Err(VmError::TypeError(
          "EventTarget.addEventListener: options.signal must be an AbortSignal",
        ))
      }
    }
  }
  let listener_id = web_events::ListenerId::from_gc_object(callback_obj);

  // SAFETY: `dom_ptr` is derived from the current `VmHost` (or the realm's fallback document) and
  // is only used for the duration of this native call.
  let dom = unsafe { dom_ptr.as_mut() };
  let added =
    dom
      .events_mut()
      .add_event_listener(resolved.target_id, &type_name, listener_id, options);

  // Root the callback while it's registered so it survives GC.
  let roots = get_or_create_event_listener_roots(scope, listener_roots_owner)?;
  let listener_key = listener_id_property_key(scope, listener_id)?;
  scope.push_root(callback)?;
  scope.define_property(roots, listener_key, data_desc(callback))?;

  if added {
    if let Some(signal_obj) = signal_obj {
      // Attach an abort algorithm that removes this listener when the signal is aborted.
      //
      // We approximate the DOM abort-algorithm list by registering an internal `{ once: true }`
      // capture listener on the signal's `abort` event. Capture ensures the cleanup runs before
      // bubble listeners, closer to the spec's "run abort algorithms, then dispatch" ordering.
      let abort_cleanup_call_id = event_target_abort_cleanup_call_id_from_callee(scope, callee)?;

      // Resolve the AbortSignal as an EventTarget so we register in the correct listener registry.
      scope.push_root(Value::Object(signal_obj))?;
      let signal_target = resolve_event_target(vm, scope, host, callee, signal_obj)?;
      let ResolvedEventTarget {
        resolved: signal_resolved,
        dom_ptr: mut signal_dom_ptr,
        listener_roots_owner: signal_roots_owner,
        ..
      } = signal_target;

      let (target_kind_slot, target_id_slot) = match resolved.target_id {
        web_events::EventTargetId::Window => (Value::Number(0.0), Value::Undefined),
        web_events::EventTargetId::Document => (Value::Number(1.0), Value::Undefined),
        web_events::EventTargetId::Node(node_id) => {
          (Value::Number(2.0), Value::Number(node_id.index() as f64))
        }
        web_events::EventTargetId::Opaque(id) => {
          let id_s = scope.alloc_string(&id.to_string())?;
          scope.push_root(Value::String(id_s))?;
          (Value::Number(3.0), Value::String(id_s))
        }
      };

      let listener_id_s = scope.alloc_string(&listener_id.get().to_string())?;
      scope.push_root(Value::String(listener_id_s))?;

      let abort_func_name = scope.alloc_string("__fastrender_abort_signal_listener_cleanup")?;
      scope.push_root(Value::String(abort_func_name))?;

      let abort_slots = [
        target_kind_slot,
        target_id_slot,
        Value::String(type_string),
        Value::String(listener_id_s),
        Value::Bool(options.capture),
      ];
      let abort_func = scope.alloc_native_function_with_slots(
        abort_cleanup_call_id,
        None,
        abort_func_name,
        0,
        &abort_slots,
      )?;

      // Register the abort cleanup listener.
      let abort_listener_id = web_events::ListenerId::from_gc_object(abort_func);
      // SAFETY: `signal_dom_ptr` is derived from the current `VmHost` (or the realm's fallback
      // document) and is only used for the duration of this native call.
      let signal_dom = unsafe { signal_dom_ptr.as_mut() };
      signal_dom.events_mut().add_event_listener(
        signal_resolved.target_id,
        "abort",
        abort_listener_id,
        web_events::AddEventListenerOptions {
          capture: true,
          once: true,
          passive: false,
        },
      );

      // Root the abort cleanup callback while it's registered.
      let roots = get_or_create_event_listener_roots(scope, signal_roots_owner)?;
      let key = listener_id_property_key(scope, abort_listener_id)?;
      scope.push_root(Value::Object(abort_func))?;
      scope.define_property(roots, key, data_desc(Value::Object(abort_func)))?;
    }
  }

  Ok(Value::Undefined)
}

fn event_target_remove_event_listener_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target_obj = event_target_resolve_this(scope, callee, this)?;
  let resolved = resolve_event_target(vm, scope, host, callee, target_obj)?;
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
  let capture = parse_event_listener_capture(vm, scope, host, hooks, options_value)?;
  let listener_id = web_events::ListenerId::from_gc_object(callback_obj);

  // SAFETY: `dom_ptr` is derived from the current `VmHost` (or the realm's fallback document) and
  // is only used for the duration of this native call.
  let dom = unsafe { dom_ptr.as_mut() };
  let removed =
    dom
      .events_mut()
      .remove_event_listener(resolved.target_id, &type_name, listener_id, capture);
  if removed {
    remove_listener_root_if_unused(scope, listener_roots_owner, dom.events(), listener_id, None)?;
  }

  Ok(Value::Undefined)
}

fn event_target_dispatch_event_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  vm_host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let target_obj = event_target_resolve_this(scope, callee, this)?;
  let resolved = resolve_event_target(vm, scope, vm_host, callee, target_obj)?;
  let ResolvedEventTarget {
    resolved,
    dom_ptr,
    opaque_target_obj,
    ..
  } = resolved;

  let event_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(event_obj) = event_value else {
    return Err(VmError::TypeError(
      "EventTarget.dispatchEvent: event is not an object",
    ));
  };
  scope.push_root(Value::Object(event_obj))?;

  let mut rust_event = rust_event_from_js_event(vm, scope, vm_host, hooks, event_obj)?;

  // Ensure base Event fields are observable even if there are no listeners.
  {
    let target_key = alloc_key(scope, "target")?;
    let target_v = match resolved.target_id {
      web_events::EventTargetId::Window => Value::Object(resolved.window_obj),
      web_events::EventTargetId::Document => Value::Object(resolved.document_obj),
      web_events::EventTargetId::Node(node_id) => {
        // SAFETY: `dom_ptr` is derived from the current JS call turn's `VmHost` context and remains
        // valid for the duration of the call.
        let dom = unsafe { dom_ptr.as_ref() };
        get_or_create_node_wrapper(vm, scope, resolved.document_obj, Some(dom), node_id)?
      }
      web_events::EventTargetId::Opaque(_) => {
        Value::Object(opaque_target_obj.ok_or_else(|| {
          VmError::InvariantViolation("opaque EventTarget is missing required JS object handle")
        })?)
      }
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
  let immediate_stop_key = alloc_key(scope, EVENT_IMMEDIATE_STOP_KEY)?;
  scope.define_property(event_obj, immediate_stop_key, data_desc(Value::Bool(false)))?;

  let document_roots = get_or_create_event_listener_roots(scope, resolved.document_obj)?;
  let mut invoker = VmJsDomEventInvoker {
    vm,
    scope,
    vm_host,
    hooks,
    window_obj: resolved.window_obj,
    document_obj: resolved.document_obj,
    event_obj,
    dom_ptr,
    document_listener_roots: document_roots,
    opaque_target_obj,
    registry: unsafe { dom_ptr.as_ref() }.events(),
  };

  // SAFETY: `dom_ptr` is derived from the current `VmHost` context and remains valid for the
  // duration of this native call.
  let dom = unsafe { dom_ptr.as_ref() };
  let mut result = web_events::dispatch_event(
    resolved.target_id,
    &mut rust_event,
    dom,
    dom.events(),
    &mut invoker,
  )
  .map_err(|_err| VmError::TypeError("EventTarget.dispatchEvent failed"))?;

  enum EventHandlerCancelOnReturn {
    True,
    False,
  }

  fn invoke_event_handler(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    vm_host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    invoker: &mut VmJsDomEventInvoker<'_, '_>,
    event_obj: GcObject,
    rust_event: &mut web_events::Event,
    current_target: web_events::EventTargetId,
    handler: Value,
    this: Value,
    args: &[Value],
    cancel_on: EventHandlerCancelOnReturn,
  ) -> Result<(), VmError> {
    if !scope.heap().is_callable(handler).unwrap_or(false) {
      return Ok(());
    }

    // Expose `currentTarget`/`eventPhase` while the handler runs.
    rust_event.current_target = Some(current_target);
    rust_event.event_phase = web_events::EventPhase::AtTarget;
    invoker.sync_event_object(rust_event)?;

    // Install the active Rust `Event` pointer so Event.prototype methods can mutate it.
    let event_id = NEXT_ACTIVE_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    let _active_guard = push_active_event_for_host(vm_host, event_id, rust_event);

    let event_id_key = alloc_key(scope, EVENT_ID_KEY)?;
    scope.define_property(event_obj, event_id_key, data_desc(Value::Number(event_id as f64)))?;

    let call_result = vm.call_with_host_and_hooks(vm_host, scope, hooks, handler, this, args);

    match call_result {
      Ok(ret) => {
        // HTML EventHandler semantics:
        // - returning `false` cancels most events
        // - `window.onerror` returning `true` cancels the error.
        let should_cancel = match cancel_on {
          EventHandlerCancelOnReturn::False => matches!(ret, Value::Bool(false)),
          EventHandlerCancelOnReturn::True => matches!(ret, Value::Bool(true)),
        };
        if should_cancel {
          rust_event.prevent_default();
        }
      }
      Err(err) => {
        // Per web platform behavior, exceptions from event handlers should not abort `dispatchEvent`.
        invoker.report_listener_exception(err);
      }
    }

    // Ensure cancellation/propagation flags set via the JS `Event` instance are reflected onto the
    // shared Rust `Event` so callers observe them even when no host-side `ActiveEventStack` is
    // installed.
    sync_rust_event_from_js_event_object(scope, event_obj, rust_event)?;

    // Restore final state.
    rust_event.event_phase = web_events::EventPhase::None;
    rust_event.current_target = None;
    Ok(())
  }

  // Window + Document EventHandler properties (`onload`, `onvisibilitychange`, ...).
  //
  // This is a minimal approximation of the web platform's EventHandler IDL attributes: after the
  // normal DOM event dispatch completes, invoke `target["on" + type]` if it is callable.
  if matches!(
    resolved.target_id,
    web_events::EventTargetId::Window | web_events::EventTargetId::Document
  ) {
    let (handler_target_obj, handler_target_id) = match resolved.target_id {
      web_events::EventTargetId::Window => (resolved.window_obj, web_events::EventTargetId::Window),
      web_events::EventTargetId::Document => (resolved.document_obj, web_events::EventTargetId::Document),
      _ => unreachable!(),
    };

    let handler_name = format!("on{}", rust_event.type_);
    let handler_key = alloc_key(scope, &handler_name)?;
    let handler = scope
      .heap()
      .object_get_own_data_property_value(handler_target_obj, &handler_key)?;

    if let Some(handler) = handler {
      if !scope.heap().is_callable(handler).unwrap_or(false) {
        // `EventHandler` attributes only fire for callable values.
        result = !rust_event.default_prevented;
      } else if handler_target_id == web_events::EventTargetId::Window && rust_event.type_ == "error" {
        // `window.onerror`: OnErrorEventHandler signature:
        //   (message, filename, lineno, colno, error) -> boolean
        //
        // Best-effort extract fields from the dispatched ErrorEvent instance (if it looks like
        // one), otherwise fall back to empty/zero values.
        let message_key = alloc_key(scope, "message")?;
        let filename_key = alloc_key(scope, "filename")?;
        let lineno_key = alloc_key(scope, "lineno")?;
        let colno_key = alloc_key(scope, "colno")?;
        let error_key = alloc_key(scope, "error")?;

        let message_v =
          vm.get_with_host_and_hooks(vm_host, scope, hooks, event_obj, message_key).unwrap_or(Value::Undefined);
        let filename_v =
          vm.get_with_host_and_hooks(vm_host, scope, hooks, event_obj, filename_key).unwrap_or(Value::Undefined);
        let lineno_v =
          vm.get_with_host_and_hooks(vm_host, scope, hooks, event_obj, lineno_key).unwrap_or(Value::Undefined);
        let colno_v =
          vm.get_with_host_and_hooks(vm_host, scope, hooks, event_obj, colno_key).unwrap_or(Value::Undefined);
        let error_v =
          vm.get_with_host_and_hooks(vm_host, scope, hooks, event_obj, error_key).unwrap_or(Value::Undefined);

        let message_s = match message_v {
          Value::Undefined | Value::Null => scope.alloc_string("")?,
          Value::String(s) => s,
          other => match scope.heap_mut().to_string(other) {
            Ok(s) => s,
            Err(_) => scope.alloc_string("")?,
          },
        };
        scope.push_root(Value::String(message_s))?;
        let filename_s = match filename_v {
          Value::Undefined | Value::Null => scope.alloc_string("")?,
          Value::String(s) => s,
          other => match scope.heap_mut().to_string(other) {
            Ok(s) => s,
            Err(_) => scope.alloc_string("")?,
          },
        };
        scope.push_root(Value::String(filename_s))?;

        let lineno = scope
          .heap_mut()
          .to_number(lineno_v)
          .map(|n| if n.is_finite() { n } else { 0.0 })
          .unwrap_or(0.0);
        let colno = scope
          .heap_mut()
          .to_number(colno_v)
          .map(|n| if n.is_finite() { n } else { 0.0 })
          .unwrap_or(0.0);

        let onerror_args = [
          Value::String(message_s),
          Value::String(filename_s),
          Value::Number(lineno),
          Value::Number(colno),
          error_v,
        ];

        invoke_event_handler(
          vm,
          scope,
          vm_host,
          hooks,
          &mut invoker,
          event_obj,
          &mut rust_event,
          handler_target_id,
          handler,
          Value::Object(handler_target_obj),
          &onerror_args,
          EventHandlerCancelOnReturn::True,
        )?;
        result = !rust_event.default_prevented;
      } else {
        let handler_args = [event_value];
        invoke_event_handler(
          vm,
          scope,
          vm_host,
          hooks,
          &mut invoker,
          event_obj,
          &mut rust_event,
          handler_target_id,
          handler,
          Value::Object(handler_target_obj),
          &handler_args,
          EventHandlerCancelOnReturn::False,
        )?;
        result = !rust_event.default_prevented;
      }
    }
  }

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
    StorageEvent,
  }

  let kind = if name.eq_ignore_ascii_case("Event") {
    Kind::Event
  } else if name.eq_ignore_ascii_case("CustomEvent") {
    Kind::CustomEvent
  } else if name.eq_ignore_ascii_case("StorageEvent") {
    Kind::StorageEvent
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
    Kind::StorageEvent => STORAGE_EVENT_PROTOTYPE_KEY,
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

  if matches!(kind, Kind::StorageEvent) {
    let key_key = alloc_key(scope, "key")?;
    scope.define_property(obj, key_key, read_only_data_desc(Value::Null))?;
    let old_value_key = alloc_key(scope, "oldValue")?;
    scope.define_property(obj, old_value_key, read_only_data_desc(Value::Null))?;
    let new_value_key = alloc_key(scope, "newValue")?;
    scope.define_property(obj, new_value_key, read_only_data_desc(Value::Null))?;
    let url_key = alloc_key(scope, "url")?;
    scope.define_property(obj, url_key, read_only_data_desc(Value::String(empty)))?;
    let storage_area_key = alloc_key(scope, "storageArea")?;
    scope.define_property(
      obj,
      storage_area_key,
      read_only_data_desc(Value::Null),
    )?;
  }

  Ok(Value::Object(obj))
}

fn node_append_child_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(parent_obj) = this else {
    return Err(VmError::TypeError(
      "Node.appendChild must be called on a node object",
    ));
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Node.appendChild requires a DOM-backed document",
  ))?;
  let mut dom_ptr = NonNull::from(&mut *dom);

  let parent_node_id = dom
    .node_id_from_index(parent_index)
    .map_err(|_| VmError::TypeError("Node.appendChild must be called on a node object"))?;
  let child_node_id = dom
    .node_id_from_index(child_index)
    .map_err(|_| VmError::TypeError("Node.appendChild requires a node argument"))?;

  let document_obj = node_wrapper_document_obj(scope, parent_obj, parent_node_id)?;
  let child_document_obj = node_wrapper_document_obj(scope, child_obj, child_node_id)
    .map_err(|_| VmError::TypeError("Node.appendChild requires a node argument"))?;
  if child_document_obj != document_obj {
    return Err(VmError::TypeError(
      "Node.appendChild cannot move nodes between documents",
    ));
  }

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

  sync_cached_child_nodes_for_wrapper(vm, scope, document_obj, dom, parent_obj, parent_node_id)?;
  if let Some(old_parent) = old_parent {
    if old_parent != parent_node_id {
      sync_cached_child_nodes_for_node_id(vm, scope, document_obj, dom, old_parent)?;
    }
  }
  if child_is_fragment {
    sync_cached_child_nodes_for_wrapper(vm, scope, document_obj, dom, child_obj, child_node_id)?;
  }

  let inserted_roots: Vec<NodeId> = if child_is_fragment {
    fragment_children
  } else {
    vec![child_node_id]
  };

  run_dynamic_script_insertion_steps(
    vm,
    scope,
    host,
    hooks,
    document_obj,
    dom_ptr,
    &inserted_roots,
  )?;
  run_dynamic_script_children_changed_steps(
    vm,
    scope,
    host,
    hooks,
    document_obj,
    dom_ptr,
    parent_node_id,
  )?;
  let needs_microtask = unsafe { dom_ptr.as_mut() }.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(child_value)
}

fn node_insert_before_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(parent_obj) = this else {
    return Err(VmError::TypeError(
      "Node.insertBefore must be called on a node object",
    ));
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
  let (reference_obj, reference_index) =
    if matches!(reference_value, Value::Null | Value::Undefined) {
      (None, None)
    } else {
      let Value::Object(reference_obj) = reference_value else {
        return Err(VmError::TypeError(
          "Node.insertBefore requires a reference node argument",
        ));
      };
      let index = match scope
        .heap()
        .object_get_own_data_property_value(reference_obj, &node_id_key)?
      {
        Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
        _ => {
          return Err(VmError::TypeError(
            "Node.insertBefore requires a reference node argument",
          ));
        }
      };
      (Some(reference_obj), Some(index))
    };

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Node.insertBefore requires a DOM-backed document",
  ))?;
  let mut dom_ptr = NonNull::from(&mut *dom);

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

  let document_obj = node_wrapper_document_obj(scope, parent_obj, parent_node_id)?;
  let new_child_document_obj =
    node_wrapper_document_obj(scope, new_child_obj, new_child_node_id)
      .map_err(|_| VmError::TypeError("Node.insertBefore requires a node argument"))?;
  if new_child_document_obj != document_obj {
    return Err(VmError::TypeError(
      "Node.insertBefore cannot move nodes between documents",
    ));
  }
  if let (Some(reference_obj), Some(reference_node_id)) = (reference_obj, reference_node_id) {
    let reference_document_obj = node_wrapper_document_obj(scope, reference_obj, reference_node_id)
      .map_err(|_| VmError::TypeError("Node.insertBefore requires a reference node argument"))?;
    if reference_document_obj != document_obj {
      return Err(VmError::TypeError(
        "Node.insertBefore cannot move nodes between documents",
      ));
    }
  }

  let old_parent = dom.parent_node(new_child_node_id);
  let new_child_is_fragment = matches!(
    &dom.node(new_child_node_id).kind,
    NodeKind::DocumentFragment
  );
  let fragment_children = if new_child_is_fragment {
    dom.node(new_child_node_id).children.clone()
  } else {
    Vec::new()
  };

  if let Err(err) = dom.insert_before(parent_node_id, new_child_node_id, reference_node_id) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  sync_cached_child_nodes_for_wrapper(vm, scope, document_obj, dom, parent_obj, parent_node_id)?;
  if let Some(old_parent) = old_parent {
    if old_parent != parent_node_id {
      sync_cached_child_nodes_for_node_id(vm, scope, document_obj, dom, old_parent)?;
    }
  }
  if new_child_is_fragment {
    sync_cached_child_nodes_for_wrapper(
      vm,
      scope,
      document_obj,
      dom,
      new_child_obj,
      new_child_node_id,
    )?;
  }

  let inserted_roots: Vec<NodeId> = if new_child_is_fragment {
    fragment_children
  } else {
    vec![new_child_node_id]
  };

  run_dynamic_script_insertion_steps(
    vm,
    scope,
    host,
    hooks,
    document_obj,
    dom_ptr,
    &inserted_roots,
  )?;
  run_dynamic_script_children_changed_steps(
    vm,
    scope,
    host,
    hooks,
    document_obj,
    dom_ptr,
    parent_node_id,
  )?;
  let needs_microtask = unsafe { dom_ptr.as_mut() }.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(new_child_value)
}

fn node_remove_child_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(parent_obj) = this else {
    return Err(VmError::TypeError(
      "Node.removeChild must be called on a node object",
    ));
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Node.removeChild requires a DOM-backed document",
  ))?;
  let mut dom_ptr = NonNull::from(&mut *dom);

  let parent_node_id = dom
    .node_id_from_index(parent_index)
    .map_err(|_| VmError::TypeError("Node.removeChild must be called on a node object"))?;
  let child_node_id = dom
    .node_id_from_index(child_index)
    .map_err(|_| VmError::TypeError("Node.removeChild requires a node argument"))?;

  let document_obj = node_wrapper_document_obj(scope, parent_obj, parent_node_id)?;
  let child_document_obj = node_wrapper_document_obj(scope, child_obj, child_node_id)
    .map_err(|_| VmError::TypeError("Node.removeChild requires a node argument"))?;
  if child_document_obj != document_obj {
    return Err(VmError::TypeError(
      "Node.removeChild cannot remove nodes between documents",
    ));
  }

  if let Err(err) = dom.remove_child(parent_node_id, child_node_id) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  sync_cached_child_nodes_for_wrapper(vm, scope, document_obj, dom, parent_obj, parent_node_id)?;

  run_dynamic_script_children_changed_steps(
    vm,
    scope,
    host,
    hooks,
    document_obj,
    dom_ptr,
    parent_node_id,
  )?;
  let needs_microtask = unsafe { dom_ptr.as_mut() }.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(child_value)
}

fn node_replace_child_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(parent_obj) = this else {
    return Err(VmError::TypeError(
      "Node.replaceChild must be called on a node object",
    ));
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Node.replaceChild requires a DOM-backed document",
  ))?;
  let mut dom_ptr = NonNull::from(&mut *dom);

  let parent_node_id = dom
    .node_id_from_index(parent_index)
    .map_err(|_| VmError::TypeError("Node.replaceChild must be called on a node object"))?;
  let new_child_node_id = dom
    .node_id_from_index(new_child_index)
    .map_err(|_| VmError::TypeError("Node.replaceChild requires a node argument"))?;
  let old_child_node_id = dom
    .node_id_from_index(old_child_index)
    .map_err(|_| VmError::TypeError("Node.replaceChild requires a node argument"))?;

  let document_obj = node_wrapper_document_obj(scope, parent_obj, parent_node_id)?;
  let new_child_document_obj =
    node_wrapper_document_obj(scope, new_child_obj, new_child_node_id)
      .map_err(|_| VmError::TypeError("Node.replaceChild requires a node argument"))?;
  if new_child_document_obj != document_obj {
    return Err(VmError::TypeError(
      "Node.replaceChild cannot move nodes between documents",
    ));
  }
  let old_child_document_obj =
    node_wrapper_document_obj(scope, old_child_obj, old_child_node_id)
      .map_err(|_| VmError::TypeError("Node.replaceChild requires a node argument"))?;
  if old_child_document_obj != document_obj {
    return Err(VmError::TypeError(
      "Node.replaceChild cannot move nodes between documents",
    ));
  }

  let old_parent = dom.parent_node(new_child_node_id);
  let new_child_is_fragment = matches!(
    &dom.node(new_child_node_id).kind,
    NodeKind::DocumentFragment
  );
  let fragment_children = if new_child_is_fragment {
    dom.node(new_child_node_id).children.clone()
  } else {
    Vec::new()
  };

  if let Err(err) = dom.replace_child(parent_node_id, new_child_node_id, old_child_node_id) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  sync_cached_child_nodes_for_wrapper(vm, scope, document_obj, dom, parent_obj, parent_node_id)?;
  if let Some(old_parent) = old_parent {
    if old_parent != parent_node_id {
      sync_cached_child_nodes_for_node_id(vm, scope, document_obj, dom, old_parent)?;
    }
  }
  if new_child_is_fragment {
    sync_cached_child_nodes_for_wrapper(
      vm,
      scope,
      document_obj,
      dom,
      new_child_obj,
      new_child_node_id,
    )?;
  }

  let inserted_roots: Vec<NodeId> = if new_child_is_fragment {
    fragment_children
  } else {
    vec![new_child_node_id]
  };

  run_dynamic_script_insertion_steps(
    vm,
    scope,
    host,
    hooks,
    document_obj,
    dom_ptr,
    &inserted_roots,
  )?;
  run_dynamic_script_children_changed_steps(
    vm,
    scope,
    host,
    hooks,
    document_obj,
    dom_ptr,
    parent_node_id,
  )?;
  let needs_microtask = unsafe { dom_ptr.as_mut() }.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(old_child_value)
}

fn node_clone_node_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let deep_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let deep = scope.heap().to_boolean(deep_val)?;

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Node.cloneNode requires a DOM-backed node",
  ))?;

  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Node.cloneNode must be called on a node object"))?;

  let document_obj = node_wrapper_document_obj(scope, wrapper_obj, node_id)?;
  let cloned = match dom.clone_node(node_id, deep) {
    Ok(cloned) => cloned,
    Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  };

  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), cloned)
}

fn node_traversal_getter(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  this: Value,
  f: impl FnOnce(&dom2::Document, NodeId) -> Option<NodeId>,
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Null);
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Null),
  };

  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::Null);
  };

  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Null),
  };

  let document_obj = match node_wrapper_document_obj(scope, wrapper_obj, node_id) {
    Ok(obj) => obj,
    Err(_) => return Ok(Value::Null),
  };

  match f(dom, node_id) {
    Some(found) => get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), found),
    None => Ok(Value::Null),
  }
}

fn node_parent_node_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  node_traversal_getter(vm, scope, host, this, |dom, node| dom.parent_node(node))
}

fn node_first_child_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  node_traversal_getter(vm, scope, host, this, |dom, node| dom.first_child(node))
}

fn node_previous_sibling_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  node_traversal_getter(vm, scope, host, this, |dom, node| {
    dom.previous_sibling(node)
  })
}

fn node_next_sibling_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  node_traversal_getter(vm, scope, host, this, |dom, node| dom.next_sibling(node))
}

fn node_node_type_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

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
  host: &mut dyn VmHost,
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
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

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
  host: &mut dyn VmHost,
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
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;
  Ok(Value::Bool(dom.is_connected_for_scripting(node_id)))
}

fn node_parent_element_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

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
  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), parent_id)
}

fn node_last_child_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

  let Some(child_id) = dom.last_child(node_id) else {
    return Ok(Value::Null);
  };
  let document_obj = node_wrapper_document_obj(scope, wrapper_obj, node_id)?;
  get_or_create_node_wrapper(vm, scope, document_obj, Some(dom), child_id)
}

fn node_has_child_nodes_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

  Ok(Value::Bool(dom.first_child(node_id).is_some()))
}

fn node_contains_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let node_id = dom_platform_mut(vm)
    .ok_or(VmError::TypeError("Illegal invocation"))?
    .require_node_id(scope.heap(), Value::Object(wrapper_obj))?;

  let other_value = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(other_value, Value::Null | Value::Undefined) {
    return Ok(Value::Bool(false));
  }

  let other_id = dom_platform_mut(vm)
    .ok_or(VmError::TypeError("Illegal invocation"))?
    .require_node_id(scope.heap(), other_value)?;

  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

  Ok(Value::Bool(
    dom.ancestors(other_id).any(|ancestor| ancestor == node_id),
  ))
}

fn node_child_nodes_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

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

  sync_child_nodes_array(vm, scope, document_obj, dom, node_id, array)?;
  Ok(Value::Object(array))
}

fn node_remove_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Node.remove must be called on a node object",
    ));
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
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

  let Some(dom) = dom_from_vm_host_mut(host) else {
    return Ok(Value::Undefined);
  };
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
  sync_cached_child_nodes_for_node_id(vm, scope, document_obj, dom, parent)?;
  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn node_text_content_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError(
    "Node.textContent requires a DOM-backed document",
  ))?;
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
        if matches!(
          &root_node.kind,
          NodeKind::Element { .. } | NodeKind::Slot { .. }
        ) && matches!(&dom.node(child).kind, NodeKind::ShadowRoot { .. })
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
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Node.textContent must be called on a node object",
    ));
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Node.textContent requires a DOM-backed document",
  ))?;
  let mut dom_ptr = NonNull::from(&mut *dom);
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Node.textContent must be called on a node object"))?;
  let document_obj = node_wrapper_document_obj(scope, wrapper_obj, node_id)?;

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
    NodeKind::Element { .. } | NodeKind::Slot { .. } => TextContentTarget::ReplaceChildren {
      preserve_shadow_roots: true,
    },
    NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => {
      TextContentTarget::ReplaceChildren {
        preserve_shadow_roots: false,
      }
    }
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
    run_dynamic_script_children_changed_steps(
      vm,
      scope,
      host,
      hooks,
      document_obj,
      dom_ptr,
      script,
    )?;
  }
  let needs_microtask = unsafe { dom_ptr.as_mut() }.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn text_data_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(text_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let text_id = dom_platform_mut(vm)
    .ok_or(VmError::TypeError("Illegal invocation"))?
    .require_text_id(scope.heap(), Value::Object(text_obj))?;
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

  let data = dom
    .text_data(text_id)
    .map_err(|_| VmError::TypeError("Illegal invocation"))?;
  Ok(Value::String(scope.alloc_string(data)?))
}

fn text_data_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(text_obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };

  let Some(document_obj) = vm
    .user_data::<WindowRealmUserData>()
    .and_then(|data| data.document_obj)
  else {
    return Ok(Value::Undefined);
  };

  let text_id = dom_platform_mut(vm)
    .ok_or(VmError::TypeError("Illegal invocation"))?
    .require_text_id(scope.heap(), Value::Object(text_obj))?;

  let new_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_value = scope.heap_mut().to_string(new_value)?;
  let new_value = scope
    .heap()
    .get_string(new_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError("Illegal invocation"))?;
  let mut dom_ptr = NonNull::from(&mut *dom);

  if let Err(err) = dom.set_text_data(text_id, &new_value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  let maybe_script_parent = dom
    .node(text_id)
    .parent
    .filter(|&parent| is_html_script_element(dom, parent));

  if let Some(parent) = maybe_script_parent {
    run_dynamic_script_children_changed_steps(
      vm,
      scope,
      host,
      hooks,
      document_obj,
      dom_ptr,
      parent,
    )?;
  }

  let needs_microtask = unsafe { dom_ptr.as_mut() }.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn element_tag_name_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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
    .require_element_id(scope.heap(), Value::Object(wrapper_obj))?;
  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError("Illegal invocation"))?;

  let tag = match &dom.node(node_id).kind {
    NodeKind::Element { tag_name, .. } => tag_name.as_str(),
    NodeKind::Slot { .. } => "slot",
    _ => return Err(VmError::TypeError("Illegal invocation")),
  };
  Ok(Value::String(
    scope.alloc_string(&tag.to_ascii_uppercase())?,
  ))
}

fn element_class_name_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Undefined);
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Undefined),
  };

  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::Undefined);
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };

  let class_name = dom.element_class_name(node_id);
  let s = scope.alloc_string(class_name)?;
  Ok(Value::String(s))
}

fn element_class_name_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Undefined);
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
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

  let host_dom_result: Option<(bool, bool)> = (|| -> Result<_, VmError> {
    let Some(host_dom) = crate::js::dom_host::dom_host_vmjs(host) else {
      return Ok(None);
    };
    let node_id = match host_dom.node_id_from_index(node_index) {
      Ok(id) => id,
      Err(_) => return Ok(Some((false, false))),
    };
    let changed = host_dom
      .set_element_class_name(node_id, &new_value)
      .map_err(|_| VmError::TypeError("failed to set Element.className"))?;
    let needs_microtask = if changed {
      host_dom.take_mutation_observer_microtask_needed()
    } else {
      false
    };
    Ok(Some((changed, needs_microtask)))
  })()?;
  if let Some((changed, needs_microtask)) = host_dom_result {
    if changed {
      maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;
    }
    return Ok(Value::Undefined);
  }

  let Some(dom) = dom_from_vm_host_mut(host) else {
    return Ok(Value::Undefined);
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };

  dom
    .set_element_class_name(node_id, &new_value)
    .map_err(|_| VmError::TypeError("failed to set Element.className"))?;

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn element_id_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Undefined);
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Undefined),
  };

  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::Undefined);
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };

  let id = dom.element_id(node_id);
  let s = scope.alloc_string(id)?;
  Ok(Value::String(s))
}

fn element_id_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Ok(Value::Undefined);
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
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

  let Some(dom) = dom_from_vm_host_mut(host) else {
    return Ok(Value::Undefined);
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };

  dom
    .set_element_id(node_id, &new_value)
    .map_err(|_| VmError::TypeError("failed to set Element.id"))?;

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

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
  dom: &dom2::Document,
  obj: GcObject,
) -> Result<Option<NodeId>, VmError> {
  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(None),
  };

  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(None),
  };

  Ok(Some(node_id))
}

fn element_reflected_string_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };

  let attr = native_slot_string(scope, callee)?;
  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::Undefined);
  };
  let Some(node_id) = dom_node_id_from_obj(scope, dom, obj)? else {
    return Ok(Value::Undefined);
  };
  let value = dom
    .get_attribute(node_id, &attr)
    .ok()
    .flatten()
    .unwrap_or("");
  Ok(Value::String(scope.alloc_string(value)?))
}

fn element_reflected_string_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => return Ok(Value::Undefined),
  };

  let attr = native_slot_string(scope, callee)?;
  let Some(dom) = dom_from_vm_host_mut(host) else {
    return Ok(Value::Undefined);
  };
  let Some(node_id) = dom_node_id_from_obj(scope, dom, obj)? else {
    return Ok(Value::Undefined);
  };

  let new_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let new_value = scope.heap_mut().to_string(new_value)?;
  let new_value = scope
    .heap()
    .get_string(new_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let mut dom_ptr = NonNull::from(&mut *dom);
  if let Err(err) = dom.set_attribute(node_id, &attr, &new_value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }
  let should_run_src_attribute_changed_steps =
    attr.eq_ignore_ascii_case("src") && is_html_script_element(dom, node_id);

  if should_run_src_attribute_changed_steps {
    run_dynamic_script_src_attribute_changed_steps(
      vm,
      scope,
      host,
      hooks,
      document_obj,
      dom_ptr,
      node_id,
    )?;
  }
  let needs_microtask = unsafe { dom_ptr.as_mut() }.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn element_reflected_bool_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };

  let attr = native_slot_string(scope, callee)?;
  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::Undefined);
  };
  let Some(node_id) = dom_node_id_from_obj(scope, dom, obj)? else {
    return Ok(Value::Undefined);
  };
  if attr == "async" && is_html_script_element(dom, node_id) {
    let force_async = dom.node(node_id).script_force_async;
    let async_attr = dom.has_attribute(node_id, "async").unwrap_or(false);
    return Ok(Value::Bool(force_async || async_attr));
  }
  Ok(Value::Bool(
    dom.has_attribute(node_id, &attr).unwrap_or(false),
  ))
}

fn element_reflected_bool_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => return Ok(Value::Undefined),
  };

  let attr = native_slot_string(scope, callee)?;
  let Some(dom) = dom_from_vm_host_mut(host) else {
    return Ok(Value::Undefined);
  };
  let Some(node_id) = dom_node_id_from_obj(scope, dom, obj)? else {
    return Ok(Value::Undefined);
  };

  let present_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let present = scope.heap().to_boolean(present_value)?;

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

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn is_html_script_element(dom: &dom2::Document, node_id: NodeId) -> bool {
  match &dom.node(node_id).kind {
    dom2::NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => {
      tag_name.eq_ignore_ascii_case("script")
        && (namespace.is_empty() || namespace == crate::dom::HTML_NAMESPACE)
    }
    _ => false,
  }
}

fn current_base_url_for_dynamic_scripts(vm: &Vm) -> Option<String> {
  vm.user_data::<WindowRealmUserData>()
    .map(|data| data.base_url.clone().unwrap_or_else(|| data.document_url.clone()))
}

// --- Dynamic <script> insertion (non-parser-inserted) --------------------------
//
// When JS/host DOM operations connect a subtree to the document, HTML requires running the "prepare
// a script" steps for each `<script>` element in the inserted subtree (tree order).
//
// FastRender's `vm-js` DOM shims are invoked with `host: &mut dyn vm_js::VmHost` pointing at the
// document (`BrowserDocumentDom2`), not the owning `BrowserTabHost`. This means we cannot directly
// call the tab's script scheduler synchronously from the binding layer.
//
// HTML script processing for dynamically inserted `<script>` elements:
// - Inline classic scripts execute synchronously during the DOM insertion/children-changed steps.
// - External scripts and module scripts are asynchronous and execute via the event loop.
//
// We therefore try to enqueue *asynchronous* script work onto the currently-installed event loop:
// - When running inside `BrowserTabHost`, forward to the tab so it can schedule via its pipeline.
// - When running inside `WindowHostState` (unit tests), queue a task that fetches/executes the
//   classic script.
struct CurrentScriptOverrideGuard {
  handle: Option<CurrentScriptStateHandle>,
  previous: Option<NodeId>,
}

impl CurrentScriptOverrideGuard {
  fn new(handle: Option<CurrentScriptStateHandle>, new_current: Option<NodeId>) -> Self {
    let previous = handle.as_ref().map(|handle| {
      let mut state = handle.borrow_mut();
      let prev = state.current_script;
      state.current_script = new_current;
      prev
    });
    Self {
      handle,
      previous: previous.flatten(),
    }
  }
}

impl Drop for CurrentScriptOverrideGuard {
  fn drop(&mut self) {
    let Some(handle) = self.handle.as_ref() else {
      return;
    };
    handle.borrow_mut().current_script = self.previous;
  }
}

fn current_script_state_handle_from_vm_host(
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
) -> Option<CurrentScriptStateHandle> {
  fn from_vm_host(host: &mut dyn VmHost) -> Option<CurrentScriptStateHandle> {
    if let Some(document) = host.as_any_mut().downcast_mut::<DocumentHostState>() {
      return Some(document.current_script_handle().clone());
    }
    if let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() {
      return Some(document.current_script_handle().clone());
    }
    if let Some(ctx) = host.as_any_mut().downcast_mut::<VmJsHostContext>() {
      return ctx.current_script_state().cloned();
    }
    None
  }

  from_vm_host(host).or_else(|| {
    let any = hooks.as_any_mut()?;
    let payload = any.downcast_mut::<VmJsHostHooksPayload>()?;
    let vm_host = payload.vm_host_mut()?;
    from_vm_host(vm_host)
  })
}

fn execute_dynamic_inline_script(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _document_obj: GcObject,
  dom_ptr: NonNull<dom2::Document>,
  script: NodeId,
  source_text: String,
) -> Result<(), VmError> {
  let state_handle = current_script_state_handle_from_vm_host(host, hooks);

  let new_current_script = {
    // SAFETY: `dom_ptr` points at the active `dom2::Document` for this JS call turn and is only used
    // within this function call.
    let dom = unsafe { dom_ptr.as_ref() };
    (dom.is_connected_for_scripting(script) && !node_root_is_shadow_root(dom, script))
      .then_some(script)
  };
  let _current_script_guard = CurrentScriptOverrideGuard::new(state_handle, new_current_script);

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let function_constructor = intr.function_constructor();

  let body_s = scope.alloc_string(&source_text)?;
  scope.push_root(Value::String(body_s))?;
  let body_value = Value::String(body_s);

  let func_value = match vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    Value::Object(function_constructor),
    Value::Undefined,
    &[body_value],
  ) {
    Ok(v) => v,
    Err(err) => {
      if crate::js::vm_error_format::vm_error_is_js_exception(&err) {
        return Ok(());
      }
      return Err(err);
    }
  };

  let Value::Object(func_obj) = func_value else {
    // Treat this as a JS exception (it should not abort the DOM operation).
    return Ok(());
  };
  scope.push_root(Value::Object(func_obj))?;

  // Call with an undefined receiver. For non-strict functions created by `Function()`, the VM's
  // normal `this` coercion will treat this as the global object (matching script execution).
  match vm.call_with_host_and_hooks(
    host,
    scope,
    hooks,
    Value::Object(func_obj),
    Value::Undefined,
    &[],
  ) {
    Ok(_) => Ok(()),
    Err(err) => {
      if crate::js::vm_error_format::vm_error_is_js_exception(&err) {
        Ok(())
      } else {
        Err(err)
      }
    }
  }
}

fn schedule_dynamic_script_via_browser_tab_host(
  hooks: &mut dyn VmHostHooks,
  script: NodeId,
  spec: crate::js::ScriptElementSpec,
) -> bool {
  let Some(event_loop) = event_loop_mut_from_hooks::<crate::api::BrowserTabHost>(hooks) else {
    return false;
  };
  let base_url_at_discovery = spec.base_url.clone();
  event_loop
    .queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
      let _ = host.register_and_schedule_dynamic_script(
        script,
        spec,
        base_url_at_discovery,
        event_loop,
      )?;
      Ok(())
    })
    .is_ok()
}

fn prepare_dynamic_script_element(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  document_obj: GcObject,
  mut dom_ptr: NonNull<dom2::Document>,
  script: NodeId,
) -> Result<(), VmError> {
  let base_url = current_base_url_for_dynamic_scripts(vm);

  let spec = {
    // SAFETY: `dom_ptr` points at the active `dom2::Document` for this JS call turn and is only used
    // within this function call.
    let dom = unsafe { dom_ptr.as_ref() };

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

    crate::js::dom_integration::build_dynamic_script_element_spec(dom, script, base_url.as_deref())
  };

  // HTML: if there is no `src` attribute and the inline text is empty, do nothing. Importantly,
  // this must *not* set the "already started" flag so later `src`/text mutations can trigger
  // preparation.
  if !spec.src_attr_present && spec.inline_text.is_empty() {
    return Ok(());
  }

  // Only participate in HTML script processing for recognized script types. Unknown types are
  // ignored and should remain eligible if later mutated into a runnable classic/module/import map
  // script.
  if !matches!(
    spec.script_type,
    ScriptType::Classic | ScriptType::Module | ScriptType::ImportMap
  ) {
    return Ok(());
  }

  // `integrity` attribute clamping: if present but too large, the metadata is invalid and the
  // script must not execute. Treat inline scripts like other "early-out" cases (e.g. empty inline
  // text): keep them eligible for later mutation.
  if spec.integrity_attr_present && spec.integrity.is_none() && !spec.src_attr_present {
    return Ok(());
  }

  // Inline classic dynamic scripts execute synchronously during insertion steps (observable by JS,
  // including `document.currentScript`).
  if spec.script_type == ScriptType::Classic && !spec.src_attr_present {
    // `nomodule` classic scripts must be suppressed when module scripts are supported.
    let supports_module_scripts = vm
      .user_data::<WindowRealmUserData>()
      .map(|data| data.module_graph.is_some())
      .unwrap_or(false);
    if supports_module_scripts && spec.nomodule_attr {
      // Still mark "already started" so later mutations do not cause execution.
      // SAFETY: `dom_ptr` points at the active `dom2::Document` for this JS call turn and is only used
      // within this function call.
      let dom = unsafe { dom_ptr.as_mut() };
      let _ = dom.set_script_already_started(script, true);
      return Ok(());
    }

    // Mark started before executing to avoid re-entrancy.
    // SAFETY: `dom_ptr` points at the active `dom2::Document` for this JS call turn and is only used
    // within this function call.
    let dom = unsafe { dom_ptr.as_mut() };
    if dom.set_script_already_started(script, true).is_err() {
      return Ok(());
    }

    execute_dynamic_inline_script(
      vm,
      scope,
      host,
      hooks,
      document_obj,
      dom_ptr,
      script,
      spec.inline_text,
    )?;
    return Ok(());
  }

  // Prefer scheduling via the BrowserTabHost pipeline when available.
  if event_loop_mut_from_hooks::<crate::api::BrowserTabHost>(hooks).is_some() {
    if schedule_dynamic_script_via_browser_tab_host(hooks, script, spec) {
      // Mark started only if we successfully queued the scheduling task.
      // SAFETY: `dom_ptr` points at the active `dom2::Document` for this JS call turn and is only used
      // within this function call.
      let dom = unsafe { dom_ptr.as_mut() };
      let _ = dom.set_script_already_started(script, true);
    }
    return Ok(());
  }

  // When running inside the standalone `WindowHostState` harness, queue classic scripts as tasks.
  // Module scripts/import maps are currently ignored by this path (leave eligible for later support).
  if event_loop_mut_from_hooks::<WindowHostState>(hooks).is_some() {
    let crate::js::ScriptElementSpec {
      src,
      src_attr_present,
      inline_text,
      nomodule_attr,
      crossorigin,
      referrer_policy,
      integrity_attr_present,
      integrity,
      script_type,
      ..
    } = spec;

    if script_type != ScriptType::Classic {
      return Ok(());
    }

    // `integrity` attribute clamping: if present but too large, treat it as invalid metadata.
    // Inline scripts should remain eligible for later mutations; external scripts are scheduled so
    // the task can surface an SRI error (consistent with other SRI failures).
    if integrity_attr_present && integrity.is_none() && !src_attr_present {
      return Ok(());
    }

    let scheduled = if src_attr_present {
      let Some(url) = src else {
        return Ok(());
      };
      let destination = if crossorigin.is_some() {
        FetchDestination::ScriptCors
      } else {
        FetchDestination::Script
      };
      queue_dynamic_script_task_external(
        hooks,
        script,
        url,
        destination,
        nomodule_attr,
        crossorigin,
        referrer_policy,
        integrity_attr_present,
        integrity,
      )
      .is_ok()
    } else {
      let source_name = format!("<script {}>", script.index());
      queue_dynamic_script_task_inline(hooks, script, source_name, inline_text, nomodule_attr)
        .is_ok()
    };

    if scheduled {
      // Mark started only if we successfully queued the script task.
      // SAFETY: `dom_ptr` points at the active `dom2::Document` for this JS call turn and is only used
      // within this function call.
      let dom = unsafe { dom_ptr.as_mut() };
      let _ = dom.set_script_already_started(script, true);
    }
  }

  Ok(())
}

fn run_dynamic_script_insertion_steps(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  document_obj: GcObject,
  dom_ptr: NonNull<dom2::Document>,
  inserted_roots: &[NodeId],
) -> Result<(), VmError> {
  let scripts = {
    // SAFETY: `dom_ptr` points at the active `dom2::Document` for this JS call turn and is only used
    // within this function call.
    let dom = unsafe { dom_ptr.as_ref() };
    crate::js::dom_integration::collect_inserted_script_elements(dom, inserted_roots)
  };

  for script in scripts {
    prepare_dynamic_script_element(vm, scope, host, hooks, document_obj, dom_ptr, script)?;
  }
  Ok(())
}

fn run_dynamic_script_children_changed_steps(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  document_obj: GcObject,
  dom_ptr: NonNull<dom2::Document>,
  node_id: NodeId,
) -> Result<(), VmError> {
  prepare_dynamic_script_element(vm, scope, host, hooks, document_obj, dom_ptr, node_id)
}

fn run_dynamic_script_src_attribute_changed_steps(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  document_obj: GcObject,
  dom_ptr: NonNull<dom2::Document>,
  node_id: NodeId,
) -> Result<(), VmError> {
  prepare_dynamic_script_element(vm, scope, host, hooks, document_obj, dom_ptr, node_id)
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
    .and_then(crate::resource::ReferrerPolicy::parse_value_list);
  let fetch_priority = dom
    .get_attribute(script, "fetchpriority")
    .ok()
    .flatten()
    .and_then(super::take_bounded_script_attribute_value);

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
    force_async: dom.node(script).script_force_async,
    defer_attr,
    nomodule_attr,
    crossorigin,
    integrity_attr_present,
    integrity,
    referrer_policy,
    fetch_priority,
    parser_inserted: false,
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
  hooks: &mut dyn VmHostHooks,
  script: NodeId,
  source_name: String,
  source_text: String,
  nomodule_attr: bool,
) -> Result<(), VmError> {
  let Some(event_loop) = event_loop_mut_from_hooks::<WindowHostState>(hooks) else {
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
        (dom.is_connected_for_scripting(script) && !node_root_is_shadow_root(dom, script))
          .then_some(script)
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
  hooks: &mut dyn VmHostHooks,
  script: NodeId,
  url: String,
  destination: FetchDestination,
  nomodule_attr: bool,
  crossorigin: Option<CorsMode>,
  referrer_policy: Option<ReferrerPolicy>,
  integrity_attr_present: bool,
  integrity: Option<String>,
) -> Result<(), VmError> {
  let Some(event_loop) = event_loop_mut_from_hooks::<WindowHostState>(hooks) else {
    return Ok(());
  };

  event_loop
    .queue_task(TaskSource::Script, move |host, event_loop| {
      if host.js_execution_options().supports_module_scripts && nomodule_attr {
        return Ok(());
      }

      let doc_origin = origin_from_url(&host.document_url);
      let target_origin = origin_from_url(&url);

      // Subresource Integrity (SRI) enforcement mirrors BrowserTabHost: when `integrity` is present
      // we must reject invalid metadata and verify the fetched bytes.
      if integrity_attr_present {
        if integrity.is_none() {
          return Err(crate::error::Error::Other(format!(
            "SRI blocked script {url}: integrity attribute exceeded max length of {} bytes",
            crate::js::sri::MAX_INTEGRITY_ATTRIBUTE_BYTES
          )));
        }

        // HTML requires a CORS-enabled fetch for cross-origin resources when SRI is used. Mirror the
        // BrowserTabHost behavior by requiring an explicit `crossorigin` attribute when the script
        // URL is cross-origin.
        if crossorigin.is_none() {
          if let (Some(doc_origin), Some(target_origin)) =
            (doc_origin.as_ref(), target_origin.as_ref())
          {
            if !doc_origin.same_origin(target_origin) {
              return Err(crate::error::Error::Other(format!(
                "SRI blocked script {url}: cross-origin integrity requires a CORS-enabled fetch (missing crossorigin attribute)"
              )));
            }
          }
        }
      }

      let options = host.js_execution_options();
      let context = format!("source=external url={url}");

      let mut req = FetchRequest::new(&url, destination).with_referrer_url(&host.document_url);
      if let Some(origin) = doc_origin.as_ref() {
        req = req.with_client_origin(origin);
      }
      if let Some(referrer_policy) = referrer_policy {
        req = req.with_referrer_policy(referrer_policy);
      }
      if let Some(cors_mode) = crossorigin {
        req = req.with_credentials_mode(cors_mode.credentials_mode());
      }

      let max_fetch = options.max_script_bytes.saturating_add(1);
      let res = host.fetcher().fetch_partial_with_request(req, max_fetch)?;
      options.check_script_source_bytes(res.bytes.len(), &context)?;

      ensure_http_success(&res, &url)?;
      ensure_script_mime_sane(&res, &url)?;
      if let Some(cors_mode) = crossorigin {
        if cors_enforcement_enabled() {
          ensure_cors_allows_origin(doc_origin.as_ref(), &res, &url, cors_mode)?;
        }
      }
      if integrity_attr_present {
        let integrity = integrity.as_deref().expect("integrity should be present");
        crate::js::sri::verify_integrity(&res.bytes, integrity).map_err(|message| {
          crate::error::Error::Other(format!("SRI blocked script {url}: {message}"))
        })?;
      }

      let fallback_encoding = host
        .dom()
        .get_attribute(script, "charset")
        .ok()
        .flatten()
        .map(crate::js::trim_ascii_whitespace)
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
      let source_text = crate::js::script_encoding::decode_classic_script_bytes(
        &res.bytes,
        res.content_type.as_deref(),
        fallback_encoding,
      );
      host.js_execution_options().check_script_source(&source_text, &context)?;

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

fn prepare_dynamic_script(
  dom: &mut dom2::Document,
  script: NodeId,
  base_url: &Option<String>,
  hooks: &mut dyn VmHostHooks,
) -> Result<(), VmError> {
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

  // Dynamic script preparation must enqueue tasks onto the HTML-like event loop. When this
  // `WindowRealm` is embedded without an active `EventLoop<WindowHostState>` (e.g. alternate host
  // types used by some test harnesses), we cannot schedule the script task/fetch. In that case,
  // treat this as a no-op and do **not** set "already started" so another integration layer can
  // prepare the script later.
  if event_loop_mut_from_hooks::<WindowHostState>(hooks).is_none() {
    return Ok(());
  }

  if dom.set_script_already_started(script, true).is_err() {
    return Ok(());
  }

  // `integrity` attribute clamping: if present but too large, the metadata is invalid and the script
  // must not execute.
  if spec.integrity_attr_present && spec.integrity.is_none() {
    return Ok(());
  }

  // Only classic scripts are executed by this vm-js DOM integration helper for now.
  if spec.script_type != ScriptType::Classic {
    return Ok(());
  }

  // If an `integrity` attribute exceeds our bounded parsing limit, treat it as invalid metadata (per
  // HTML) and skip execution.
  if spec.integrity_attr_present && spec.integrity.is_none() {
    return Ok(());
  }

  if spec.src_attr_present {
    if let Some(url) = spec.src {
      let destination = if spec.crossorigin.is_some() {
        FetchDestination::ScriptCors
      } else {
        FetchDestination::Script
      };
      return queue_dynamic_script_task_external(
        hooks,
        script,
        url,
        destination,
        spec.nomodule_attr,
        spec.crossorigin,
        spec.referrer_policy,
        spec.integrity_attr_present,
        spec.integrity,
      );
    }
    return Ok(());
  }

  // Inline script: queue as a task to keep DOM mutation calls non-reentrant.
  let source_name = format!("<script {}>", script.index());
  let source_text = spec.inline_text;
  queue_dynamic_script_task_inline(hooks, script, source_name, source_text, spec.nomodule_attr)
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
  hooks: &mut dyn VmHostHooks,
) -> Result<(), VmError> {
  let mut scripts = Vec::new();
  collect_html_script_elements(dom, inserted_root, &mut scripts);
  for script in scripts {
    prepare_dynamic_script(dom, script, base_url, hooks)?;
  }
  Ok(())
}

fn css_style_get_property_value_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let name_value = scope.heap_mut().to_string(name_value)?;
  let name = scope
    .heap()
    .get_string(name_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let Some(node_id) = dom_node_id_from_obj(scope, dom, style_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };

  let value = dom.style_get_property_value(node_id, &name);
  Ok(Value::String(scope.alloc_string(&value)?))
}

fn css_style_set_property_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(style_obj) = this else {
    return Err(VmError::TypeError(
      "CSSStyleDeclaration.setProperty must be called on a style object",
    ));
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(style_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => return Ok(Value::Undefined),
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(style_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Undefined),
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

  let host_dom_result: Option<(bool, bool)> = (|| -> Result<_, VmError> {
    let Some(host_dom) = crate::js::dom_host::dom_host_vmjs(host) else {
      return Ok(None);
    };
    let node_id = match host_dom.node_id_from_index(node_index) {
      Ok(id) => id,
      Err(_) => return Ok(Some((false, false))),
    };
    let changed = match host_dom.style_set_property(node_id, &name, &value) {
      Ok(changed) => changed,
      Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
    };
    let needs_microtask = if changed {
      host_dom.take_mutation_observer_microtask_needed()
    } else {
      false
    };
    Ok(Some((changed, needs_microtask)))
  })()?;
  if let Some((changed, needs_microtask)) = host_dom_result {
    if changed {
      maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;
    }
    return Ok(Value::Undefined);
  }

  let Some(dom) = dom_from_vm_host_mut(host) else {
    return Ok(Value::Undefined);
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };
  if let Err(err) = dom.style_set_property(node_id, &name, &value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn css_style_remove_property_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(style_obj) = this else {
    return Err(VmError::TypeError(
      "CSSStyleDeclaration.removeProperty must be called on a style object",
    ));
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(style_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => return Ok(Value::String(scope.alloc_string("")?)),
  };

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(style_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::String(scope.alloc_string("")?)),
  };

  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let name_value = scope.heap_mut().to_string(name_value)?;
  let name = scope
    .heap()
    .get_string(name_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let host_dom_result: Option<(Value, bool, bool)> = (|| -> Result<_, VmError> {
    let Some(host_dom) = crate::js::dom_host::dom_host_vmjs(host) else {
      return Ok(None);
    };
    let node_id = match host_dom.node_id_from_index(node_index) {
      Ok(id) => id,
      Err(_) => return Ok(Some((Value::String(scope.alloc_string("")?), false, false))),
    };
    let prev = host_dom.style_get_property_value(node_id, &name);
    let changed = match host_dom.style_set_property(node_id, &name, "") {
      Ok(changed) => changed,
      Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
    };
    let needs_microtask = if changed {
      host_dom.take_mutation_observer_microtask_needed()
    } else {
      false
    };
    Ok(Some((
      Value::String(scope.alloc_string(&prev)?),
      changed,
      needs_microtask,
    )))
  })()?;
  if let Some((value, changed, needs_microtask)) = host_dom_result {
    if changed {
      maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;
    }
    return Ok(value);
  }

  let Some(dom) = dom_from_vm_host_mut(host) else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::String(scope.alloc_string("")?)),
  };
  let prev = dom.style_get_property_value(node_id, &name);
  if let Err(err) = dom.style_set_property(node_id, &name, "") {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::String(scope.alloc_string(&prev)?))
}

fn css_style_named_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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
  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let Some(node_id) = dom_node_id_from_obj(scope, dom, style_obj)? else {
    return Ok(Value::String(scope.alloc_string("")?));
  };
  let value = dom.style_get_property_value(node_id, &prop);
  Ok(Value::String(scope.alloc_string(&value)?))
}

fn css_style_named_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(style_obj) = this else {
    return Err(VmError::TypeError(
      "CSSStyleDeclaration property setter must be called on a style object",
    ));
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(style_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => return Ok(Value::Undefined),
  };

  let prop = native_slot_string(scope, callee)?;

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  let node_index = match scope
    .heap()
    .object_get_own_data_property_value(style_obj, &node_id_key)?
  {
    Some(Value::Number(n)) if n.is_finite() && n >= 0.0 => n as usize,
    _ => return Ok(Value::Undefined),
  };

  let value_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let value_value = scope.heap_mut().to_string(value_value)?;
  let value = scope
    .heap()
    .get_string(value_value)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let host_dom_result: Option<(bool, bool)> = (|| -> Result<_, VmError> {
    let Some(host_dom) = crate::js::dom_host::dom_host_vmjs(host) else {
      return Ok(None);
    };
    let node_id = match host_dom.node_id_from_index(node_index) {
      Ok(id) => id,
      Err(_) => return Ok(Some((false, false))),
    };
    let changed = match host_dom.style_set_property(node_id, &prop, &value) {
      Ok(changed) => changed,
      Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
    };
    let needs_microtask = if changed {
      host_dom.take_mutation_observer_microtask_needed()
    } else {
      false
    };
    Ok(Some((changed, needs_microtask)))
  })()?;
  if let Some((changed, needs_microtask)) = host_dom_result {
    if changed {
      maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;
    }
    return Ok(Value::Undefined);
  }

  let Some(dom) = dom_from_vm_host_mut(host) else {
    return Ok(Value::Undefined);
  };
  let node_id = match dom.node_id_from_index(node_index) {
    Ok(id) => id,
    Err(_) => return Ok(Value::Undefined),
  };
  if let Err(err) = dom.style_set_property(node_id, &prop, &value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn element_class_list_add_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
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

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.add must be called on a classList object",
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

  let host_dom_result: Option<(bool, bool)> = (|| -> Result<_, VmError> {
    let Some(host_dom) = crate::js::dom_host::dom_host_vmjs(host) else {
      return Ok(None);
    };
    let node_id = match host_dom.node_id_from_index(node_index) {
      Ok(id) => id,
      Err(_) => {
        return Err(VmError::TypeError(
          "DOMTokenList.add must be called on a classList object",
        ));
      }
    };
    let changed = match host_dom.class_list_add(node_id, &token_refs) {
      Ok(changed) => changed,
      Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
    };
    let needs_microtask = if changed {
      host_dom.take_mutation_observer_microtask_needed()
    } else {
      false
    };
    Ok(Some((changed, needs_microtask)))
  })()?;
  if let Some((changed, needs_microtask)) = host_dom_result {
    if changed {
      maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;
    }
    return Ok(Value::Undefined);
  }

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "DOMTokenList.add requires a DOM-backed document",
  ))?;
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("DOMTokenList.add must be called on a classList object"))?;

  match dom.class_list_add(node_id, &token_refs) {
    Ok(_) => {
      let needs_microtask = dom.take_mutation_observer_microtask_needed();
      maybe_queue_mutation_observer_microtask(
        vm,
        scope,
        host,
        hooks,
        document_obj,
        needs_microtask,
      )?;
      Ok(Value::Undefined)
    }
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_class_list_remove_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
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

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.remove must be called on a classList object",
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

  let host_dom_result: Option<(bool, bool)> = (|| -> Result<_, VmError> {
    let Some(host_dom) = crate::js::dom_host::dom_host_vmjs(host) else {
      return Ok(None);
    };
    let node_id = match host_dom.node_id_from_index(node_index) {
      Ok(id) => id,
      Err(_) => {
        return Err(VmError::TypeError(
          "DOMTokenList.remove must be called on a classList object",
        ));
      }
    };
    let changed = match host_dom.class_list_remove(node_id, &token_refs) {
      Ok(changed) => changed,
      Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
    };
    let needs_microtask = if changed {
      host_dom.take_mutation_observer_microtask_needed()
    } else {
      false
    };
    Ok(Some((changed, needs_microtask)))
  })()?;
  if let Some((changed, needs_microtask)) = host_dom_result {
    if changed {
      maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;
    }
    return Ok(Value::Undefined);
  }

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "DOMTokenList.remove requires a DOM-backed document",
  ))?;
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("DOMTokenList.remove must be called on a classList object"))?;

  match dom.class_list_remove(node_id, &token_refs) {
    Ok(_) => {
      let needs_microtask = dom.take_mutation_observer_microtask_needed();
      maybe_queue_mutation_observer_microtask(
        vm,
        scope,
        host,
        hooks,
        document_obj,
        needs_microtask,
      )?;
      Ok(Value::Undefined)
    }
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_class_list_contains_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError(
    "DOMTokenList.contains requires a DOM-backed document",
  ))?;
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("DOMTokenList.contains must be called on a classList object")
  })?;

  match dom.class_list_contains(node_id, &token) {
    Ok(result) => Ok(Value::Bool(result)),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_class_list_toggle_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(class_list_obj) = this else {
    return Err(VmError::TypeError(
      "DOMTokenList.toggle must be called on a classList object",
    ));
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.toggle must be called on a classList object",
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

  let host_dom_result: Option<(bool, bool, bool)> = (|| -> Result<_, VmError> {
    let before_present = if force.is_some() {
      let dom = dom_from_vm_host(host).ok_or(VmError::TypeError(
        "DOMTokenList.toggle requires a DOM-backed document",
      ))?;
      let node_id = dom
        .node_id_from_index(node_index)
        .map_err(|_| VmError::TypeError("DOMTokenList.toggle must be called on a classList object"))?;
      match dom.class_list_contains(node_id, &token) {
        Ok(v) => v,
        Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
      }
    } else {
      false
    };
 
    let Some(host_dom) = crate::js::dom_host::dom_host_vmjs(host) else {
      return Ok(None);
    };
    let node_id = match host_dom.node_id_from_index(node_index) {
      Ok(id) => id,
      Err(_) => {
        return Err(VmError::TypeError(
          "DOMTokenList.toggle must be called on a classList object",
        ));
      }
    };
    let result = match host_dom.class_list_toggle(node_id, &token, force) {
      Ok(result) => result,
      Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
    };
    let changed = if force.is_some() { result != before_present } else { true };
    let needs_microtask = if changed {
      host_dom.take_mutation_observer_microtask_needed()
    } else {
      false
    };
    Ok(Some((result, changed, needs_microtask)))
  })()?;
  if let Some((result, changed, needs_microtask)) = host_dom_result {
    if changed {
      maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;
    }
    return Ok(Value::Bool(result));
  }

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "DOMTokenList.toggle requires a DOM-backed document",
  ))?;
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("DOMTokenList.toggle must be called on a classList object"))?;

  let result = match dom.class_list_toggle(node_id, &token, force) {
    Ok(result) => result,
    Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  };

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Bool(result))
}

fn element_class_list_replace_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(class_list_obj) = this else {
    return Err(VmError::TypeError(
      "DOMTokenList.replace must be called on a classList object",
    ));
  };

  let document_obj_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(class_list_obj, &document_obj_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "DOMTokenList.replace must be called on a classList object",
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

  let host_dom_result: Option<(bool, bool, bool)> = (|| -> Result<_, VmError> {
    let Some(host_dom) = crate::js::dom_host::dom_host_vmjs(host) else {
      return Ok(None);
    };
    let node_id = match host_dom.node_id_from_index(node_index) {
      Ok(id) => id,
      Err(_) => {
        return Err(VmError::TypeError(
          "DOMTokenList.replace must be called on a classList object",
        ));
      }
    };
    let result = match host_dom.class_list_replace(node_id, &token, &new_token) {
      Ok(result) => result,
      Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
    };
    let changed = result && token != new_token;
    let needs_microtask = if changed {
      host_dom.take_mutation_observer_microtask_needed()
    } else {
      false
    };
    Ok(Some((result, changed, needs_microtask)))
  })()?;
  if let Some((result, changed, needs_microtask)) = host_dom_result {
    if changed {
      maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;
    }
    return Ok(Value::Bool(result));
  }

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "DOMTokenList.replace requires a DOM-backed document",
  ))?;
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("DOMTokenList.replace must be called on a classList object"))?;

  let result = match dom.class_list_replace(node_id, &token, &new_token) {
    Ok(result) => result,
    Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  };

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Bool(result))
}

fn element_get_attribute_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError(
    "Element.getAttribute requires a DOM-backed document",
  ))?;
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
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.setAttribute must be called on an element object",
    ));
  };

  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.setAttribute must be called on an element object",
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.setAttribute requires a DOM-backed document",
  ))?;
  let mut dom_ptr = NonNull::from(&mut *dom);
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.setAttribute must be called on an element object"))?;
  if let Err(err) = dom.set_attribute(node_id, &name, &value) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }
  let should_run_src_attribute_changed_steps =
    name.eq_ignore_ascii_case("src") && is_html_script_element(dom, node_id);

  if should_run_src_attribute_changed_steps {
    run_dynamic_script_src_attribute_changed_steps(
      vm,
      scope,
      host,
      hooks,
      document_obj,
      dom_ptr,
      node_id,
    )?;
  }
  let needs_microtask = unsafe { dom_ptr.as_mut() }.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn element_remove_attribute_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.removeAttribute must be called on an element object",
    ));
  };

  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.removeAttribute must be called on an element object",
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.removeAttribute requires a DOM-backed document",
  ))?;
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("Element.removeAttribute must be called on an element object")
  })?;
  let mut dom_ptr = NonNull::from(&mut *dom);

  if let Err(err) = dom.remove_attribute(node_id, &name) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }
  let should_run_src_attribute_changed_steps =
    name.eq_ignore_ascii_case("src") && is_html_script_element(dom, node_id);

  if should_run_src_attribute_changed_steps {
    run_dynamic_script_src_attribute_changed_steps(
      vm,
      scope,
      host,
      hooks,
      document_obj,
      dom_ptr,
      node_id,
    )?;
  }
  let needs_microtask = unsafe { dom_ptr.as_mut() }.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn element_inner_html_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError(
    "Element.innerHTML requires a DOM-backed document",
  ))?;
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.innerHTML must be called on an element object"))?;

  match dom.inner_html(node_id) {
    Ok(html) => Ok(Value::String(scope.alloc_string(&html)?)),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_inner_html_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.innerHTML must be called on an element object",
    ));
  };

  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.innerHTML must be called on an element object",
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.innerHTML requires a DOM-backed document",
  ))?;
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.innerHTML must be called on an element object"))?;

  if let Err(err) = dom.set_inner_html(node_id, &html) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn element_outer_html_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
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

  let dom = dom_from_vm_host(host).ok_or(VmError::TypeError(
    "Element.outerHTML requires a DOM-backed document",
  ))?;
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.outerHTML must be called on an element object"))?;

  match dom.outer_html(node_id) {
    Ok(html) => Ok(Value::String(scope.alloc_string(&html)?)),
    Err(err) => Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  }
}

fn element_outer_html_set_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.outerHTML must be called on an element object",
    ));
  };

  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.outerHTML must be called on an element object",
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.outerHTML requires a DOM-backed document",
  ))?;
  let node_id = dom
    .node_id_from_index(node_index)
    .map_err(|_| VmError::TypeError("Element.outerHTML must be called on an element object"))?;

  if let Err(err) = dom.set_outer_html(node_id, &html) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn element_insert_adjacent_html_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentHTML must be called on an element object",
    ));
  };

  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentHTML must be called on an element object",
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.insertAdjacentHTML requires a DOM-backed document",
  ))?;
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("Element.insertAdjacentHTML must be called on an element object")
  })?;

  if let Err(err) = dom.insert_adjacent_html(node_id, &position, &html) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn element_insert_adjacent_element_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentElement must be called on an element object",
    ));
  };

  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentElement must be called on an element object",
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

  let child_document_obj = match scope
    .heap()
    .object_get_own_data_property_value(element_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentElement requires an element argument",
      ));
    }
  };
  if child_document_obj != document_obj {
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.insertAdjacentElement requires a DOM-backed document",
  ))?;
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("Element.insertAdjacentElement must be called on an element object")
  })?;
  let child_node_id = dom.node_id_from_index(child_index).map_err(|_| {
    VmError::TypeError("Element.insertAdjacentElement requires an element argument")
  })?;

  let result = match dom.insert_adjacent_element(node_id, &position, child_node_id) {
    Ok(Some(_)) => element_value,
    Ok(None) => Value::Null,
    Err(err) => return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?)),
  };

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(result)
}

fn element_insert_adjacent_text_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(wrapper_obj) = this else {
    return Err(VmError::TypeError(
      "Element.insertAdjacentText must be called on an element object",
    ));
  };

  let wrapper_document_key = alloc_key(scope, WRAPPER_DOCUMENT_KEY)?;
  let document_obj = match scope
    .heap()
    .object_get_own_data_property_value(wrapper_obj, &wrapper_document_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      return Err(VmError::TypeError(
        "Element.insertAdjacentText must be called on an element object",
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

  let dom = dom_from_vm_host_mut(host).ok_or(VmError::TypeError(
    "Element.insertAdjacentText requires a DOM-backed document",
  ))?;
  let node_id = dom.node_id_from_index(node_index).map_err(|_| {
    VmError::TypeError("Element.insertAdjacentText must be called on an element object")
  })?;

  if let Err(err) = dom.insert_adjacent_text(node_id, &position, &text) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  let needs_microtask = dom.take_mutation_observer_microtask_needed();
  maybe_queue_mutation_observer_microtask(vm, scope, host, hooks, document_obj, needs_microtask)?;

  Ok(Value::Undefined)
}

fn document_current_script_get_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::Null);
  };

  fn current_script_from_vm_host(host: &mut dyn VmHost) -> Option<NodeId> {
    if let Some(document) = host
      .as_any_mut()
      .downcast_mut::<crate::js::host_document::DocumentHostState>()
    {
      return document.current_script_handle().borrow().current_script;
    }
    if let Some(document) = host.as_any_mut().downcast_mut::<BrowserDocumentDom2>() {
      return document.current_script_handle().borrow().current_script;
    }
    if let Some(ctx) = host.as_any_mut().downcast_mut::<VmJsHostContext>() {
      return ctx
        .current_script_state()
        .and_then(|handle| handle.borrow().current_script);
    }
    None
  }

  let node_id = current_script_from_vm_host(host).or_else(|| {
    let any = hooks.as_any_mut()?;
    let payload = any.downcast_mut::<VmJsHostHooksPayload>()?;
    let vm_host = payload.vm_host_mut()?;
    current_script_from_vm_host(vm_host)
  });

  let Some(node_id) = node_id else {
    return Ok(Value::Null);
  };
  let dom = dom_from_vm_host(host);
  get_or_create_node_wrapper(vm, scope, document_obj, dom, node_id)
}

fn document_ready_state_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(document_obj) = this else {
    return Ok(Value::String(scope.alloc_string("complete")?));
  };
  let _ = document_obj;

  let Some(dom) = dom_from_vm_host(host) else {
    return Ok(Value::String(scope.alloc_string("complete")?));
  };

  Ok(Value::String(
    scope.alloc_string(dom.ready_state().as_str())?,
  ))
}

fn document_visibility_state_from_vm_host(
  host: &mut dyn VmHost,
) -> crate::web::dom::DocumentVisibilityState {
  use std::any::TypeId;

  let any = host.as_any_mut();
  let ty = any.type_id();
  let ptr = any as *mut dyn std::any::Any;

  // SAFETY: we only cast the erased `Any` pointer back to a concrete type after checking its
  // runtime `TypeId`.
  unsafe {
    if ty == TypeId::of::<BrowserDocumentDom2>() {
      let host = &mut *(ptr as *mut BrowserDocumentDom2);
      return host.visibility_state();
    }
  }

  crate::web::dom::DocumentVisibilityState::Visible
}

fn document_visibility_state_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(_document_obj) = this else {
    return Ok(Value::String(scope.alloc_string("visible")?));
  };

  let state = document_visibility_state_from_vm_host(host);
  Ok(Value::String(scope.alloc_string(state.as_str())?))
}

fn document_hidden_get_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(_document_obj) = this else {
    return Ok(Value::Bool(false));
  };

  let state = document_visibility_state_from_vm_host(host);
  Ok(Value::Bool(state.hidden()))
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
  // `document.baseURI` reflects the document base URL used to resolve relative URLs. When the
  // embedder has not yet installed a base URL, fall back to the realm's document URL.
  let base_url = vm
    .user_data_mut::<WindowRealmUserData>()
    .map(|data| {
      data
        .base_url
        .clone()
        .unwrap_or_else(|| data.document_url.clone())
    })
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

fn document_write_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  document_write_impl(vm, scope, host, hooks, args, false)
}

fn document_writeln_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  document_write_impl(vm, scope, host, hooks, args, true)
}

fn document_write_impl(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  args: &[Value],
  append_newline: bool,
) -> Result<Value, VmError> {
  let call_name = if append_newline {
    "document.writeln"
  } else {
    "document.write"
  };

  fn document_write_state_ptr_from_hooks(
    hooks: &mut dyn VmHostHooks,
  ) -> Option<*mut crate::js::DocumentWriteState> {
    if let Some(state) = current_document_write_state_mut() {
      return Some(state as *mut _);
    }
    let any = hooks.as_any_mut()?;
    // `Any::downcast_mut` returns a mutable borrow of `any` that would escape through the returned
    // pointer. Use a raw pointer to avoid borrow checker issues while trying multiple downcast
    // targets.
    let any_ptr: *mut dyn std::any::Any = any;
    // SAFETY: `any_ptr` is derived from `hooks.as_any_mut()` and is only used within this function.
    unsafe {
      let payload = (&mut *any_ptr).downcast_mut::<VmJsHostHooksPayload>()?;
      let host = payload.embedder_state_mut::<BrowserTabHost>()?;
      Some(host.document_write_state_mut() as *mut _)
    }
  }

  fn shared_diagnostics_from_vm_host(host: &mut dyn VmHost) -> Option<crate::api::SharedRenderDiagnostics> {
    host
      .as_any_mut()
      .downcast_mut::<BrowserDocumentDom2>()
      .and_then(|document| document.shared_diagnostics())
  }

  fn shared_diagnostics_from_vm_host_or_hooks(
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Option<crate::api::SharedRenderDiagnostics> {
    shared_diagnostics_from_vm_host(host).or_else(|| {
      let any = hooks.as_any_mut()?;
      let any_ptr: *mut dyn std::any::Any = any;
      // SAFETY: `any_ptr` is derived from `hooks.as_any_mut()` and is only used within this block.
      unsafe {
        let payload = (&mut *any_ptr).downcast_mut::<VmJsHostHooksPayload>()?;
        let vm_host = payload.vm_host_mut()?;
        shared_diagnostics_from_vm_host(vm_host)
      }
    })
  }

  fn warn_document_write(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    message: &str,
  ) {
    // Best-effort: warnings must never throw.
    let console_warned = (|| -> Result<bool, VmError> {
      let Some(window_obj) = vm.user_data::<WindowRealmUserData>().and_then(|data| data.window_obj) else {
        return Ok(false);
      };

      let console_key = alloc_key(scope, "console")?;
      let Some(Value::Object(console_obj)) = scope
        .heap()
        .object_get_own_data_property_value(window_obj, &console_key)?
      else {
        return Ok(false);
      };

      let sink_key = alloc_key(scope, CONSOLE_SINK_ID_KEY)?;
      let Some(Value::Number(n)) = scope
        .heap()
        .object_get_own_data_property_value(console_obj, &sink_key)?
      else {
        return Ok(false);
      };
      if !n.is_finite() || n < 0.0 {
        return Ok(false);
      }
      let sink_id = n as u64;
      let sink = console_sinks().lock().get(&sink_id).cloned();
      let Some(sink) = sink else {
        return Ok(false);
      };

      let msg_s = scope.alloc_string(message)?;
      scope.push_root(Value::String(msg_s))?;
      sink(ConsoleMessageLevel::Warn, scope.heap_mut(), &[Value::String(msg_s)]);
      Ok(true)
    })()
    .unwrap_or(false);

    if console_warned {
      return;
    }

    if let Some(diag) = shared_diagnostics_from_vm_host_or_hooks(host, hooks) {
      diag.record_console_message(ConsoleMessageLevel::Warn, message.to_string());
    }
  }

  let state_ptr = document_write_state_ptr_from_hooks(hooks);
  let max_bytes_per_call = state_ptr
    .map(|ptr| unsafe { (&*ptr).max_bytes_per_call() })
    .unwrap_or_else(|| JsExecutionOptions::default().max_document_write_bytes_per_call);

  let mut out = String::new();
  for &arg in args {
    let s_handle = match arg {
      Value::String(s) => s,
      other => scope.heap_mut().to_string(other)?,
    };
    let s = scope.heap().get_string(s_handle)?;
    if s.as_code_units().len() > max_bytes_per_call.saturating_sub(out.len()) {
      warn_document_write(
        vm,
        scope,
        host,
        hooks,
        &format!(
          "{call_name} ignored: exceeded max bytes per call (limit={max_bytes_per_call})"
        ),
      );
      return Ok(Value::Undefined);
    }
    out.push_str(&s.to_utf8_lossy());
    if out.len() > max_bytes_per_call {
      warn_document_write(
        vm,
        scope,
        host,
        hooks,
        &format!(
          "{call_name} ignored: exceeded max bytes per call (limit={max_bytes_per_call})"
        ),
      );
      return Ok(Value::Undefined);
    }
  }

  if append_newline {
    if out.len() >= max_bytes_per_call {
      warn_document_write(
        vm,
        scope,
        host,
        hooks,
        &format!(
          "{call_name} ignored: exceeded max bytes per call (limit={max_bytes_per_call})"
        ),
      );
      return Ok(Value::Undefined);
    }
    out.push('\n');
  }

  let parsing_active = state_ptr.is_some_and(|ptr| unsafe { (&*ptr).parsing_active() });
  let can_inject = parsing_active && crate::html::document_write::has_active_streaming_parser();

  if let Some(state_ptr) = state_ptr {
    // SAFETY: `state_ptr` points at either:
    // - the TLS-installed `DocumentWriteState` for this JS call boundary, or
    // - the embedder `BrowserTabHost`'s `document_write_state` while it is mutably borrowed by the
    //   event loop task currently running JS.
    //
    // Keep the `&mut DocumentWriteState` borrow scoped to this call so we don't accidentally alias
    // it with other `hooks`/`host` accesses while emitting warnings.
    let write_result = unsafe { (&mut *state_ptr).try_write(&out, can_inject) };
    match write_result {
      Ok(()) => {
        if !can_inject {
          warn_document_write(
            vm,
            scope,
            host,
            hooks,
            &format!(
              "{call_name} ignored: no active streaming HTML parser (post-parse deterministic no-op)"
            ),
          );
        }
      }
      Err(DocumentWriteLimitError::TooManyCalls { limit }) => {
        warn_document_write(
          vm,
          scope,
          host,
          hooks,
          &format!("{call_name} ignored: exceeded max call count (limit={limit})"),
        );
      }
      Err(DocumentWriteLimitError::PerCallBytesExceeded { len, limit }) => {
        warn_document_write(
          vm,
          scope,
          host,
          hooks,
          &format!("{call_name} ignored: exceeded max bytes per call (len={len}, limit={limit})"),
        );
      }
      Err(DocumentWriteLimitError::TotalBytesExceeded {
        current,
        add,
        limit,
      }) => {
        warn_document_write(
          vm,
          scope,
          host,
          hooks,
          &format!(
            "{call_name} ignored: exceeded max cumulative bytes (current={current}, add={add}, limit={limit})"
          ),
        );
      }
    }
  } else {
    // Deterministic subset of HTML's ignore-destructive-writes behavior:
    // when no streaming parser is active, treat `document.write()` as a no-op instead of
    // implicitly calling `document.open()` and clearing the document.
    warn_document_write(
      vm,
      scope,
      host,
      hooks,
      &format!(
        "{call_name} ignored: no active streaming HTML parser (post-parse deterministic no-op)"
      ),
    );
  }

  Ok(Value::Undefined)
}

fn init_window_globals(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
  config: &WindowRealmConfig,
) -> Result<(Option<u64>, Option<u64>), VmError> {
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

  // Location stringification.
  //
  // Real pages often do `String(location)` or `location + ''` expecting the current URL; provide
  // native stringifier methods that return the internal `href` string.
  let location_to_string_call_id = vm.register_native_call(location_to_string_native)?;
  let to_string_key = alloc_key(&mut scope, "toString")?;
  let to_string_name = scope.alloc_string("toString")?;
  scope.push_root(Value::String(to_string_name))?;
  let to_string_func =
    scope.alloc_native_function(location_to_string_call_id, None, to_string_name, 0)?;
  scope.heap_mut().object_set_prototype(
    to_string_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(to_string_func))?;
  scope.define_property(
    location_obj,
    to_string_key,
    data_desc(Value::Object(to_string_func)),
  )?;

  let value_of_key = alloc_key(&mut scope, "valueOf")?;
  let value_of_name = scope.alloc_string("valueOf")?;
  scope.push_root(Value::String(value_of_name))?;
  let value_of_func =
    scope.alloc_native_function(location_to_string_call_id, None, value_of_name, 0)?;
  scope.heap_mut().object_set_prototype(
    value_of_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(value_of_func))?;
  scope.define_property(
    location_obj,
    value_of_key,
    data_desc(Value::Object(value_of_func)),
  )?;

  let location_to_primitive_call_id = vm.register_native_call(location_to_primitive_native)?;
  let to_primitive_key = PropertyKey::from_symbol(realm.well_known_symbols().to_primitive);
  let to_primitive_name = scope.alloc_string("[Symbol.toPrimitive]")?;
  scope.push_root(Value::String(to_primitive_name))?;
  let to_primitive_func =
    scope.alloc_native_function(location_to_primitive_call_id, None, to_primitive_name, 1)?;
  scope.heap_mut().object_set_prototype(
    to_primitive_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(to_primitive_func))?;
  scope.define_property(
    location_obj,
    to_primitive_key,
    data_desc(Value::Object(to_primitive_func)),
  )?;

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
  scope.define_property(
    location_obj,
    assign_key,
    data_desc(Value::Object(assign_func)),
  )?;

  let replace_call_id = vm.register_native_call(location_replace_native)?;
  let replace_name = scope.alloc_string("replace")?;
  scope.push_root(Value::String(replace_name))?;
  let replace_func = scope.alloc_native_function(replace_call_id, None, replace_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(replace_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(replace_func))?;
  scope.define_property(
    location_obj,
    replace_key,
    data_desc(Value::Object(replace_func)),
  )?;

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

  let mut dom_platform = Some(DomPlatform::new(&mut scope, realm)?);

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

  // Backreference to the realm's `window` global. This is used by internal helpers (for example
  // mutation observer microtask scheduling) that need to reach the window from the document.
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

  let mutation_observer_registry = scope.alloc_object()?;
  scope.push_root(Value::Object(mutation_observer_registry))?;
  let mutation_observer_registry_key = alloc_key(&mut scope, MUTATION_OBSERVER_REGISTRY_KEY)?;
  scope.define_property(
    document_obj,
    mutation_observer_registry_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(mutation_observer_registry),
        writable: false,
      },
    },
  )?;

  let mutation_observer_notify_call_id =
    vm.register_native_call(mutation_observer_notify_native)?;
  let mutation_observer_notify_name = scope.alloc_string("notify mutation observers")?;
  scope.push_root(Value::String(mutation_observer_notify_name))?;
  let mutation_observer_notify_func = scope.alloc_native_function_with_slots(
    mutation_observer_notify_call_id,
    None,
    mutation_observer_notify_name,
    0,
    &[Value::Object(document_obj)],
  )?;
  scope.heap_mut().object_set_prototype(
    mutation_observer_notify_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(mutation_observer_notify_func))?;
  let mutation_observer_notify_key = alloc_key(&mut scope, MUTATION_OBSERVER_NOTIFY_KEY)?;
  scope.define_property(
    document_obj,
    mutation_observer_notify_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Object(mutation_observer_notify_func),
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

  // `Document.defaultView`.
  //
  // Real-world scripts often use this as a stable reference to the `window` object associated with
  // the document (e.g. `document.defaultView.requestAnimationFrame(...)`).
  let default_view_key = alloc_key(&mut scope, "defaultView")?;
  scope.define_property(
    document_obj,
    default_view_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(global),
        writable: false,
      },
    },
  )?;

  // Document.title.
  //
  // Many real-world scripts read and write this value (e.g. analytics metadata, SPA routing).
  // FastRender does not yet sync the title back into the DOM tree; expose a writable string slot so
  // scripts do not crash when accessing `document.title`.
  let title_key = alloc_key(&mut scope, "title")?;
  let title_s = scope.alloc_string("")?;
  scope.push_root(Value::String(title_s))?;
  scope.define_property(
    document_obj,
    title_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::String(title_s),
        writable: true,
      },
    },
  )?;

  // Document.characterSet / Document.charset / Document.inputEncoding.
  //
  // Real-world scripts often use these to decide on encoding-sensitive behaviour. FastRender always
  // decodes HTML/script resources as UTF-8 unless explicitly overridden by `charset` attributes or
  // response headers, so default to UTF-8 here.
  let encoding_s = scope.alloc_string("UTF-8")?;
  scope.push_root(Value::String(encoding_s))?;
  for prop in ["characterSet", "charset", "inputEncoding"] {
    let key = alloc_key(&mut scope, prop)?;
    scope.define_property(
      document_obj,
      key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::String(encoding_s),
          writable: false,
        },
      },
    )?;
  }

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
  let visibility_state_call_id = vm.register_native_call(document_visibility_state_get_native)?;
  let visibility_state_name = scope.alloc_string("get visibilityState")?;
  scope.push_root(Value::String(visibility_state_name))?;
  let visibility_state_func =
    scope.alloc_native_function(visibility_state_call_id, None, visibility_state_name, 0)?;
  scope.heap_mut().object_set_prototype(
    visibility_state_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(visibility_state_func))?;
  scope.define_property(
    document_obj,
    visibility_state_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(visibility_state_func),
        set: Value::Undefined,
      },
    },
  )?;

  let hidden_key = alloc_key(&mut scope, "hidden")?;
  let hidden_call_id = vm.register_native_call(document_hidden_get_native)?;
  let hidden_name = scope.alloc_string("get hidden")?;
  scope.push_root(Value::String(hidden_name))?;
  let hidden_func = scope.alloc_native_function(hidden_call_id, None, hidden_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(hidden_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(hidden_func))?;
  scope.define_property(
    document_obj,
    hidden_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Accessor {
        get: Value::Object(hidden_func),
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
  scope
    .heap_mut()
    .object_set_prototype(write_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(write_func))?;
  scope.define_property(
    document_obj,
    write_key,
    data_desc(Value::Object(write_func)),
  )?;

  let writeln_key = alloc_key(&mut scope, "writeln")?;
  let writeln_call_id = vm.register_native_call(document_writeln_native)?;
  let writeln_name = scope.alloc_string("writeln")?;
  scope.push_root(Value::String(writeln_name))?;
  let writeln_func = scope.alloc_native_function(writeln_call_id, None, writeln_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(writeln_func, Some(realm.intrinsics().function_prototype()))?;
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

  // Document.textContent (from Node): always null.
  let document_text_content_key = alloc_key(&mut scope, "textContent")?;
  let document_text_content_get_call_id =
    vm.register_native_call(document_text_content_get_native)?;
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

  let document_text_content_set_call_id =
    vm.register_native_call(document_text_content_set_native)?;
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
  scope
    .heap_mut()
    .object_set_prototype(write_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(write_func))?;
  scope.define_property(
    document_obj,
    write_key,
    data_desc(Value::Object(write_func)),
  )?;

  let writeln_key = alloc_key(&mut scope, "writeln")?;
  let writeln_call_id = vm.register_native_call(document_writeln_native)?;
  let writeln_name = scope.alloc_string("writeln")?;
  scope.push_root(Value::String(writeln_name))?;
  let writeln_func = scope.alloc_native_function(writeln_call_id, None, writeln_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(writeln_func, Some(realm.intrinsics().function_prototype()))?;
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
  let create_fragment_call_id =
    vm.register_native_call(document_create_document_fragment_native)?;
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

  // --- DOM Events (MVP): Event / CustomEvent / StorageEvent / document.createEvent --------------
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

  let storage_event_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(storage_event_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(storage_event_proto, Some(event_proto))?;

  let init_storage_event_call_id = vm.register_native_call(storage_event_init_storage_event_native)?;
  let init_storage_event_name = scope.alloc_string("initStorageEvent")?;
  scope.push_root(Value::String(init_storage_event_name))?;
  let init_storage_event_func =
    scope.alloc_native_function(init_storage_event_call_id, None, init_storage_event_name, 8)?;
  scope.heap_mut().object_set_prototype(
    init_storage_event_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(init_storage_event_func))?;
  let init_storage_event_key = alloc_key(&mut scope, "initStorageEvent")?;
  scope.define_property(
    storage_event_proto,
    init_storage_event_key,
    data_desc(Value::Object(init_storage_event_func)),
  )?;

  let promise_rejection_event_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(promise_rejection_event_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(promise_rejection_event_proto, Some(event_proto))?;

  let error_event_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(error_event_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(error_event_proto, Some(event_proto))?;

  let before_unload_event_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(before_unload_event_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(before_unload_event_proto, Some(event_proto))?;

  let page_transition_event_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(page_transition_event_proto))?;
  scope
    .heap_mut()
    .object_set_prototype(page_transition_event_proto, Some(event_proto))?;

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

  // DOM Event phase constants (Event.NONE .. Event.BUBBLING_PHASE). These are WebIDL `const`s and
  // must be non-writable/non-configurable.
  for (name, value) in [
    ("NONE", 0.0),
    ("CAPTURING_PHASE", 1.0),
    ("AT_TARGET", 2.0),
    ("BUBBLING_PHASE", 3.0),
  ] {
    let key = alloc_key(&mut scope, name)?;
    scope.define_property(
      event_ctor_func,
      key,
      non_configurable_read_only_data_desc(Value::Number(value)),
    )?;
    scope.define_property(
      event_proto,
      key,
      non_configurable_read_only_data_desc(Value::Number(value)),
    )?;
  }

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

  let storage_event_ctor_call_id = vm.register_native_call(storage_event_constructor_native)?;
  let storage_event_ctor_construct_id =
    vm.register_native_construct(storage_event_constructor_construct_native)?;
  let storage_event_ctor_name = scope.alloc_string("StorageEvent")?;
  scope.push_root(Value::String(storage_event_ctor_name))?;
  let storage_event_ctor_func = scope.alloc_native_function(
    storage_event_ctor_call_id,
    Some(storage_event_ctor_construct_id),
    storage_event_ctor_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    storage_event_ctor_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(storage_event_ctor_func))?;
  scope.define_property(
    storage_event_ctor_func,
    prototype_key,
    data_desc(Value::Object(storage_event_proto)),
  )?;
  scope.define_property(
    storage_event_proto,
    constructor_key,
    data_desc(Value::Object(storage_event_ctor_func)),
  )?;
  let storage_event_ctor_key = alloc_key(&mut scope, "StorageEvent")?;
  scope.define_property(
    global,
    storage_event_ctor_key,
    data_desc(Value::Object(storage_event_ctor_func)),
  )?;

  let promise_rejection_event_ctor_call_id =
    vm.register_native_call(promise_rejection_event_constructor_native)?;
  let promise_rejection_event_ctor_construct_id =
    vm.register_native_construct(promise_rejection_event_constructor_construct_native)?;
  let promise_rejection_event_ctor_name = scope.alloc_string("PromiseRejectionEvent")?;
  scope.push_root(Value::String(promise_rejection_event_ctor_name))?;
  let promise_rejection_event_ctor_func = scope.alloc_native_function(
    promise_rejection_event_ctor_call_id,
    Some(promise_rejection_event_ctor_construct_id),
    promise_rejection_event_ctor_name,
    2,
  )?;
  scope.heap_mut().object_set_prototype(
    promise_rejection_event_ctor_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(promise_rejection_event_ctor_func))?;
  scope.define_property(
    promise_rejection_event_ctor_func,
    prototype_key,
    data_desc(Value::Object(promise_rejection_event_proto)),
  )?;
  scope.define_property(
    promise_rejection_event_proto,
    constructor_key,
    data_desc(Value::Object(promise_rejection_event_ctor_func)),
  )?;
  let promise_rejection_event_ctor_key = alloc_key(&mut scope, "PromiseRejectionEvent")?;
  scope.define_property(
    global,
    promise_rejection_event_ctor_key,
    data_desc(Value::Object(promise_rejection_event_ctor_func)),
  )?;

  let error_event_ctor_call_id = vm.register_native_call(error_event_constructor_native)?;
  let error_event_ctor_construct_id =
    vm.register_native_construct(error_event_constructor_construct_native)?;
  let error_event_ctor_name = scope.alloc_string("ErrorEvent")?;
  scope.push_root(Value::String(error_event_ctor_name))?;
  let error_event_ctor_func = scope.alloc_native_function(
    error_event_ctor_call_id,
    Some(error_event_ctor_construct_id),
    error_event_ctor_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    error_event_ctor_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(error_event_ctor_func))?;
  scope.define_property(
    error_event_ctor_func,
    prototype_key,
    data_desc(Value::Object(error_event_proto)),
  )?;
  scope.define_property(
    error_event_proto,
    constructor_key,
    data_desc(Value::Object(error_event_ctor_func)),
  )?;
  let error_event_ctor_key = alloc_key(&mut scope, "ErrorEvent")?;
  scope.define_property(
    global,
    error_event_ctor_key,
    data_desc(Value::Object(error_event_ctor_func)),
  )?;

  let before_unload_event_ctor_call_id =
    vm.register_native_call(before_unload_event_constructor_native)?;
  let before_unload_event_ctor_construct_id =
    vm.register_native_construct(before_unload_event_constructor_construct_native)?;
  let before_unload_event_ctor_name = scope.alloc_string("BeforeUnloadEvent")?;
  scope.push_root(Value::String(before_unload_event_ctor_name))?;
  let before_unload_event_ctor_func = scope.alloc_native_function(
    before_unload_event_ctor_call_id,
    Some(before_unload_event_ctor_construct_id),
    before_unload_event_ctor_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    before_unload_event_ctor_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(before_unload_event_ctor_func))?;
  scope.define_property(
    before_unload_event_ctor_func,
    prototype_key,
    data_desc(Value::Object(before_unload_event_proto)),
  )?;
  scope.define_property(
    before_unload_event_proto,
    constructor_key,
    data_desc(Value::Object(before_unload_event_ctor_func)),
  )?;
  let before_unload_event_ctor_key = alloc_key(&mut scope, "BeforeUnloadEvent")?;
  scope.define_property(
    global,
    before_unload_event_ctor_key,
    data_desc(Value::Object(before_unload_event_ctor_func)),
  )?;

  let page_transition_event_ctor_call_id =
    vm.register_native_call(page_transition_event_constructor_native)?;
  let page_transition_event_ctor_construct_id =
    vm.register_native_construct(page_transition_event_constructor_construct_native)?;
  let page_transition_event_ctor_name = scope.alloc_string("PageTransitionEvent")?;
  scope.push_root(Value::String(page_transition_event_ctor_name))?;
  let page_transition_event_ctor_func = scope.alloc_native_function(
    page_transition_event_ctor_call_id,
    Some(page_transition_event_ctor_construct_id),
    page_transition_event_ctor_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    page_transition_event_ctor_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(page_transition_event_ctor_func))?;
  scope.define_property(
    page_transition_event_ctor_func,
    prototype_key,
    data_desc(Value::Object(page_transition_event_proto)),
  )?;
  scope.define_property(
    page_transition_event_proto,
    constructor_key,
    data_desc(Value::Object(page_transition_event_ctor_func)),
  )?;
  let page_transition_event_ctor_key = alloc_key(&mut scope, "PageTransitionEvent")?;
  scope.define_property(
    global,
    page_transition_event_ctor_key,
    data_desc(Value::Object(page_transition_event_ctor_func)),
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
  let storage_event_proto_key = alloc_key(&mut scope, STORAGE_EVENT_PROTOTYPE_KEY)?;
  scope.define_property(
    document_obj,
    storage_event_proto_key,
    data_desc(Value::Object(storage_event_proto)),
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
  let abort_cleanup_call_id = vm.register_native_call(abort_signal_listener_cleanup_native)?;
  let add_event_listener_name = scope.alloc_string("addEventListener")?;
  scope.push_root(Value::String(add_event_listener_name))?;
  // EventTarget method native slots:
  // - slot 0: default `this` for global functions (see `event_target_resolve_this`)
  // - slot 1: the realm's global object (used to find `document` for non-DOM EventTargets like
  //   `AbortSignal` / `new EventTarget()`).
  // - slot 2: native call id for the internal AbortSignal cleanup callback used by
  //   `addEventListener(..., { signal })`.
  let event_target_global_slots = [
    Value::Object(global),
    Value::Object(global),
    Value::Number(abort_cleanup_call_id.0 as f64),
  ];
  let event_target_method_slots = [
    Value::Undefined,
    Value::Object(global),
    Value::Number(abort_cleanup_call_id.0 as f64),
  ];
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
  let event_target_proto = match dom_platform.as_ref() {
    Some(platform) => platform.prototype_for(DomInterface::EventTarget),
    None => scope.alloc_object()?,
  };
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
    1,
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
      scope
        .heap_mut()
        .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
      Ok(func)
    };

    // Node constructor + constants.
    let node_ctor = make_illegal_ctor(&mut scope, "Node")?;
    scope.push_root(Value::Object(node_ctor))?;
    scope.define_property(
      node_ctor,
      prototype_key,
      data_desc(Value::Object(node_proto)),
    )?;
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
    scope.define_property(global, element_key, data_desc(Value::Object(element_ctor)))?;

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
    scope.define_property(
      text_ctor,
      prototype_key,
      data_desc(Value::Object(text_proto)),
    )?;
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
    let parent_element_get_func =
      scope.alloc_native_function(parent_element_get_call_id, None, parent_element_get_name, 0)?;
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
    scope
      .heap_mut()
      .object_set_prototype(contains_func, Some(realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(contains_func))?;
    let contains_key = alloc_key(&mut scope, "contains")?;
    scope.define_property(
      node_proto,
      contains_key,
      data_desc(Value::Object(contains_func)),
    )?;

    let has_child_nodes_call_id = vm.register_native_call(node_has_child_nodes_native)?;
    let has_child_nodes_name = scope.alloc_string("hasChildNodes")?;
    scope.push_root(Value::String(has_child_nodes_name))?;
    let has_child_nodes_func =
      scope.alloc_native_function(has_child_nodes_call_id, None, has_child_nodes_name, 0)?;
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

  // MutationObserver constructor + prototype.
  let mutation_observer_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(mutation_observer_proto))?;

  let mutation_observer_observe_call_id =
    vm.register_native_call(mutation_observer_observe_native)?;
  let mutation_observer_observe_name = scope.alloc_string("observe")?;
  scope.push_root(Value::String(mutation_observer_observe_name))?;
  let mutation_observer_observe_func = scope.alloc_native_function(
    mutation_observer_observe_call_id,
    None,
    mutation_observer_observe_name,
    2,
  )?;
  scope.heap_mut().object_set_prototype(
    mutation_observer_observe_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(mutation_observer_observe_func))?;
  let mutation_observer_observe_key = alloc_key(&mut scope, "observe")?;
  scope.define_property(
    mutation_observer_proto,
    mutation_observer_observe_key,
    data_desc(Value::Object(mutation_observer_observe_func)),
  )?;

  let mutation_observer_disconnect_call_id =
    vm.register_native_call(mutation_observer_disconnect_native)?;
  let mutation_observer_disconnect_name = scope.alloc_string("disconnect")?;
  scope.push_root(Value::String(mutation_observer_disconnect_name))?;
  let mutation_observer_disconnect_func = scope.alloc_native_function(
    mutation_observer_disconnect_call_id,
    None,
    mutation_observer_disconnect_name,
    0,
  )?;
  scope.heap_mut().object_set_prototype(
    mutation_observer_disconnect_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(mutation_observer_disconnect_func))?;
  let mutation_observer_disconnect_key = alloc_key(&mut scope, "disconnect")?;
  scope.define_property(
    mutation_observer_proto,
    mutation_observer_disconnect_key,
    data_desc(Value::Object(mutation_observer_disconnect_func)),
  )?;

  let mutation_observer_take_records_call_id =
    vm.register_native_call(mutation_observer_take_records_native)?;
  let mutation_observer_take_records_name = scope.alloc_string("takeRecords")?;
  scope.push_root(Value::String(mutation_observer_take_records_name))?;
  let mutation_observer_take_records_func = scope.alloc_native_function(
    mutation_observer_take_records_call_id,
    None,
    mutation_observer_take_records_name,
    0,
  )?;
  scope.heap_mut().object_set_prototype(
    mutation_observer_take_records_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(mutation_observer_take_records_func))?;
  let mutation_observer_take_records_key = alloc_key(&mut scope, "takeRecords")?;
  scope.define_property(
    mutation_observer_proto,
    mutation_observer_take_records_key,
    data_desc(Value::Object(mutation_observer_take_records_func)),
  )?;

  let mutation_observer_ctor_call_id =
    vm.register_native_call(mutation_observer_constructor_native)?;
  let mutation_observer_ctor_construct_id =
    vm.register_native_construct(mutation_observer_constructor_construct_native)?;
  let mutation_observer_ctor_name = scope.alloc_string("MutationObserver")?;
  scope.push_root(Value::String(mutation_observer_ctor_name))?;
  let mutation_observer_ctor_func = scope.alloc_native_function(
    mutation_observer_ctor_call_id,
    Some(mutation_observer_ctor_construct_id),
    mutation_observer_ctor_name,
    1,
  )?;
  scope.heap_mut().object_set_prototype(
    mutation_observer_ctor_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(mutation_observer_ctor_func))?;
  scope.define_property(
    mutation_observer_ctor_func,
    prototype_key,
    data_desc(Value::Object(mutation_observer_proto)),
  )?;
  scope.define_property(
    mutation_observer_proto,
    constructor_key,
    data_desc(Value::Object(mutation_observer_ctor_func)),
  )?;
  let mutation_observer_key = alloc_key(&mut scope, "MutationObserver")?;
  scope.define_property(
    global,
    mutation_observer_key,
    data_desc(Value::Object(mutation_observer_ctor_func)),
  )?;

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
  let append_child_public_key = alloc_key(&mut scope, "appendChild")?;
  scope.define_property(
    document_obj,
    append_child_public_key,
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
  let insert_before_public_key = alloc_key(&mut scope, "insertBefore")?;
  scope.define_property(
    document_obj,
    insert_before_public_key,
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
  let remove_child_public_key = alloc_key(&mut scope, "removeChild")?;
  scope.define_property(
    document_obj,
    remove_child_public_key,
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
  let replace_child_public_key = alloc_key(&mut scope, "replaceChild")?;
  scope.define_property(
    document_obj,
    replace_child_public_key,
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
  let clone_node_public_key = alloc_key(&mut scope, "cloneNode")?;
  scope.define_property(
    document_obj,
    clone_node_public_key,
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
  let text_content_get_func =
    scope.alloc_native_function(text_content_get_call_id, None, text_content_get_name, 0)?;
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
  let text_content_set_func =
    scope.alloc_native_function(text_content_set_call_id, None, text_content_set_name, 1)?;
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
  let remove_attribute_func =
    scope.alloc_native_function(remove_attribute_call_id, None, remove_attribute_name, 1)?;
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
  let reflected_string_get_call_id =
    vm.register_native_call(element_reflected_string_get_native)?;
  let reflected_string_set_call_id =
    vm.register_native_call(element_reflected_string_set_native)?;
  let reflected_bool_get_call_id = vm.register_native_call(element_reflected_bool_get_native)?;
  let reflected_bool_set_call_id = vm.register_native_call(element_reflected_bool_set_native)?;

  for (prop, attr, get_key_name, set_key_name) in [
    ("src", "src", ELEMENT_SRC_GET_KEY, ELEMENT_SRC_SET_KEY),
    (
      "srcset",
      "srcset",
      ELEMENT_SRCSET_GET_KEY,
      ELEMENT_SRCSET_SET_KEY,
    ),
    (
      "sizes",
      "sizes",
      ELEMENT_SIZES_GET_KEY,
      ELEMENT_SIZES_SET_KEY,
    ),
    ("href", "href", ELEMENT_HREF_GET_KEY, ELEMENT_HREF_SET_KEY),
    ("rel", "rel", ELEMENT_REL_GET_KEY, ELEMENT_REL_SET_KEY),
    ("type", "type", ELEMENT_TYPE_GET_KEY, ELEMENT_TYPE_SET_KEY),
    (
      "charset",
      "charset",
      ELEMENT_CHARSET_GET_KEY,
      ELEMENT_CHARSET_SET_KEY,
    ),
    (
      "crossOrigin",
      "crossorigin",
      ELEMENT_CROSS_ORIGIN_GET_KEY,
      ELEMENT_CROSS_ORIGIN_SET_KEY,
    ),
    (
      "height",
      "height",
      ELEMENT_HEIGHT_GET_KEY,
      ELEMENT_HEIGHT_SET_KEY,
    ),
    (
      "width",
      "width",
      ELEMENT_WIDTH_GET_KEY,
      ELEMENT_WIDTH_SET_KEY,
    ),
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
    (
      "async",
      "async",
      ELEMENT_ASYNC_GET_KEY,
      ELEMENT_ASYNC_SET_KEY,
    ),
    (
      "defer",
      "defer",
      ELEMENT_DEFER_GET_KEY,
      ELEMENT_DEFER_SET_KEY,
    ),
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
  let console_noop_call_id = vm.register_native_call(console_noop_native)?;
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
      let func =
        scope.alloc_native_function_with_slots(console_call_id, None, name_s, 0, &slots)?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
      scope.push_root(Value::Object(func))?;
      Ok(Value::Object(func))
    };

  let define_console_noop_method =
    |scope: &mut Scope<'_>, name: &str| -> Result<Value, VmError> {
      let name_s = scope.alloc_string(name)?;
      scope.push_root(Value::String(name_s))?;
      let func = scope.alloc_native_function(console_noop_call_id, None, name_s, 0)?;
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
  let trace_key = alloc_key(&mut scope, "trace")?;
  let group_key = alloc_key(&mut scope, "group")?;
  let group_collapsed_key = alloc_key(&mut scope, "groupCollapsed")?;
  let group_end_key = alloc_key(&mut scope, "groupEnd")?;
  let clear_key = alloc_key(&mut scope, "clear")?;

  let log_func = define_console_method(&mut scope, "log", ConsoleMessageLevel::Log)?;
  let info_func = define_console_method(&mut scope, "info", ConsoleMessageLevel::Info)?;
  let warn_func = define_console_method(&mut scope, "warn", ConsoleMessageLevel::Warn)?;
  let error_func = define_console_method(&mut scope, "error", ConsoleMessageLevel::Error)?;
  let debug_func = define_console_method(&mut scope, "debug", ConsoleMessageLevel::Debug)?;
  let trace_func = define_console_method(&mut scope, "trace", ConsoleMessageLevel::Debug)?;
  let group_func = define_console_noop_method(&mut scope, "group")?;
  let group_collapsed_func = define_console_noop_method(&mut scope, "groupCollapsed")?;
  let group_end_func = define_console_noop_method(&mut scope, "groupEnd")?;
  let clear_func = define_console_noop_method(&mut scope, "clear")?;

  scope.define_property(console_obj, log_key, data_desc(log_func))?;
  scope.define_property(console_obj, info_key, data_desc(info_func))?;
  scope.define_property(console_obj, warn_key, data_desc(warn_func))?;
  scope.define_property(console_obj, error_key, data_desc(error_func))?;
  scope.define_property(console_obj, debug_key, data_desc(debug_func))?;
  scope.define_property(console_obj, trace_key, data_desc(trace_func))?;
  scope.define_property(console_obj, group_key, data_desc(group_func))?;
  scope.define_property(console_obj, group_collapsed_key, data_desc(group_collapsed_func))?;
  scope.define_property(console_obj, group_end_key, data_desc(group_end_func))?;
  scope.define_property(console_obj, clear_key, data_desc(clear_func))?;

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

  // --- History (window.history) ------------------------------------------------
  //
  // Many real-world sites (especially SPAs) rely on `history.pushState` / `history.replaceState`
  // for client-side routing. Provide a minimal, non-navigating History facade that updates
  // `location.href`/`document.URL` so URL-derived routing logic works.
  let history_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(history_obj))?;
  let history_key = alloc_key(&mut scope, "history")?;
  let history_state_key = alloc_key(&mut scope, "state")?;
  let history_length_key = alloc_key(&mut scope, "length")?;
  scope.define_property(
    history_obj,
    history_state_key,
    read_only_data_desc(Value::Null),
  )?;
  scope.define_property(
    history_obj,
    history_length_key,
    read_only_data_desc(Value::Number(1.0)),
  )?;

  let history_state_call_id = vm.register_native_call(history_state_change_native)?;
  let history_go_call_id = vm.register_native_call(history_go_native)?;
  let history_noop_call_id = vm.register_native_call(history_noop_native)?;

  let push_state_key = alloc_key(&mut scope, "pushState")?;
  let push_state_name = scope.alloc_string("pushState")?;
  scope.push_root(Value::String(push_state_name))?;
  let push_state_slots = [
    Value::Object(history_obj),
    Value::Object(location_obj),
    Value::Object(document_obj),
    Value::Bool(false),
  ];
  let push_state_func = scope.alloc_native_function_with_slots(
    history_state_call_id,
    None,
    push_state_name,
    2,
    &push_state_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    push_state_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(push_state_func))?;
  scope.define_property(
    history_obj,
    push_state_key,
    data_desc(Value::Object(push_state_func)),
  )?;

  let replace_state_key = alloc_key(&mut scope, "replaceState")?;
  let replace_state_name = scope.alloc_string("replaceState")?;
  scope.push_root(Value::String(replace_state_name))?;
  let replace_state_slots = [
    Value::Object(history_obj),
    Value::Object(location_obj),
    Value::Object(document_obj),
    Value::Bool(true),
  ];
  let replace_state_func = scope.alloc_native_function_with_slots(
    history_state_call_id,
    None,
    replace_state_name,
    2,
    &replace_state_slots,
  )?;
  scope.heap_mut().object_set_prototype(
    replace_state_func,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(replace_state_func))?;
  scope.define_property(
    history_obj,
    replace_state_key,
    data_desc(Value::Object(replace_state_func)),
  )?;

  let back_key = alloc_key(&mut scope, "back")?;
  let back_name = scope.alloc_string("back")?;
  scope.push_root(Value::String(back_name))?;
  let back_func = scope.alloc_native_function(history_noop_call_id, None, back_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(back_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(back_func))?;
  scope.define_property(history_obj, back_key, data_desc(Value::Object(back_func)))?;

  let forward_key = alloc_key(&mut scope, "forward")?;
  let forward_name = scope.alloc_string("forward")?;
  scope.push_root(Value::String(forward_name))?;
  let forward_func = scope.alloc_native_function(history_noop_call_id, None, forward_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(forward_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(forward_func))?;
  scope.define_property(
    history_obj,
    forward_key,
    data_desc(Value::Object(forward_func)),
  )?;

  let go_key = alloc_key(&mut scope, "go")?;
  let go_name = scope.alloc_string("go")?;
  scope.push_root(Value::String(go_name))?;
  let go_func = scope.alloc_native_function(history_go_call_id, None, go_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(go_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(go_func))?;
  scope.define_property(history_obj, go_key, data_desc(Value::Object(go_func)))?;

  scope.define_property(
    global,
    history_key,
    read_only_data_desc(Value::Object(history_obj)),
  )?;

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
    config.web_storage_quota_utf16_bytes,
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
    config.web_storage_quota_utf16_bytes,
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

  if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
    // `window.document` is writable/configurable, so store canonical handles in host-owned state for
    // receiver branding and event dispatch.
    data.window_obj = Some(global);
    data.document_obj = Some(document_obj);
    if let Some(platform) = dom_platform.take() {
      data.dom_platform = Some(platform);
    }
  }

  // Install WHATWG URL bindings (`URL`/`URLSearchParams`) so real-world scripts can parse and
  // manipulate URLs. This must happen after `scope` is dropped because it borrows `heap` mutably.
  drop(scope);
  crate::js::window_abort::install_window_abort_bindings(vm, realm, heap)?;
  crate::js::window_intersection_observer::install_window_intersection_observer_bindings(vm, realm, heap)?;
  crate::js::window_crypto::install_window_crypto_bindings(vm, realm, heap)?;
  crate::js::window_css::install_window_css_bindings(vm, realm, heap)?;
  crate::js::window_text_encoding::install_window_text_encoding_bindings(vm, realm, heap)?;
  crate::js::window_url::install_window_url_bindings(vm, realm, heap)?;
  crate::js::window_blob::install_window_blob_bindings(vm, realm, heap)?;
  crate::js::window_file::install_window_file_bindings(vm, realm, heap)?;
  crate::js::window_form_data::install_window_form_data_bindings(vm, realm, heap)?;

  Ok((
    console_sink_guard.map(ConsoleSinkGuard::disarm),
    Some(match_media_guard.disarm()),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::clock::VirtualClock;
  use crate::js::window_env::FASTRENDER_USER_AGENT;
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

  #[test]
  fn current_script_state_handle_from_vm_host_supports_vmjs_host_context() {
    let handle = CurrentScriptStateHandle::default();
    let mut host_ctx = VmJsHostContext {
      current_script_state: Some(handle.clone()),
    };
    let mut hooks = NoopHostHooks::default();

    let found = current_script_state_handle_from_vm_host(&mut host_ctx, &mut hooks)
      .expect("expected current script handle");

    let id = NodeId::from_index(42);
    found.borrow_mut().current_script = Some(id);
    assert_eq!(handle.borrow().current_script, Some(id));
  }

  /// Minimal `VmHostHooks` implementation for tests that execute scripts with a real `VmHost`
  /// context, but without an `EventLoop`.
  ///
  /// This provides the DOM shim exotic hooks (`Element.dataset`) while discarding Promise jobs.
  struct DomShimHostHooks {
    any: VmJsHostHooksPayload,
  }

  impl DomShimHostHooks {
    fn new(host_ctx: &mut dyn VmHost) -> Self {
      let mut any = VmJsHostHooksPayload::default();
      any.set_vm_host(host_ctx);
      Self { any }
    }
  }

  impl vm_js::VmHostHooks for DomShimHostHooks {
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
      Some(&mut self.any)
    }

    fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {}

    fn host_exotic_get(
      &mut self,
      scope: &mut Scope<'_>,
      obj: GcObject,
      key: PropertyKey,
      receiver: Value,
    ) -> Result<Option<Value>, VmError> {
      let _ = receiver;
      dataset_exotic_get(scope, self.any.vm_host_mut(), obj, key)
    }

    fn host_exotic_set(
      &mut self,
      scope: &mut Scope<'_>,
      obj: GcObject,
      key: PropertyKey,
      value: Value,
      receiver: Value,
    ) -> Result<Option<bool>, VmError> {
      let _ = receiver;
      dataset_exotic_set(scope, self.any.vm_host_mut(), obj, key, value)
    }

    fn host_exotic_delete(
      &mut self,
      scope: &mut Scope<'_>,
      obj: GcObject,
      key: PropertyKey,
    ) -> Result<Option<bool>, VmError> {
      dataset_exotic_delete(scope, self.any.vm_host_mut(), obj, key)
    }
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

  fn exec_script_with_dom_host(
    realm: &mut WindowRealm,
    host: &mut dyn VmHost,
    source: &str,
  ) -> Result<Value, VmError> {
    let mut hooks = DomShimHostHooks::new(host);
    realm.exec_script_with_host_and_hooks(host, &mut hooks, source)
  }

  fn new_realm(config: WindowRealmConfig) -> Result<WindowRealm, VmError> {
    let mut js_execution_options = JsExecutionOptions::default();
    // These unit tests validate DOM/Web API behaviour, not the per-run wall time budget. Increase
    // it so debug builds running tests in parallel don't trip the default budget.
    js_execution_options.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(2));
    // Keep the heap limits configured by `WindowRealmConfig` (some tests tweak it).
    js_execution_options.max_vm_heap_bytes = None;
    WindowRealm::new_with_js_execution_options(config, js_execution_options)
  }

  fn check_hooks_payload_native(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Some(any) = hooks.as_any_mut() else {
      return Err(VmError::TypeError("VmHostHooks::as_any_mut returned None"));
    };
    let Some(payload) = any.downcast_mut::<VmJsHostHooksPayload>() else {
      return Err(VmError::TypeError(
        "VmHostHooks::as_any_mut did not downcast to VmJsHostHooksPayload",
      ));
    };
    if payload.vm_host_mut().is_none() {
      return Err(VmError::TypeError(
        "VmJsHostHooksPayload did not contain a VmHost pointer",
      ));
    }
    Ok(Value::Undefined)
  }

  #[test]
  fn exec_script_with_name_exposes_vmjs_host_hooks_payload() -> Result<(), VmError> {
    let mut window = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      scope.push_root(Value::Object(global))?;

      let call_id = vm.register_native_call(check_hooks_payload_native)?;
      let name_s = scope.alloc_string("__check_hooks_payload")?;
      scope.push_root(Value::String(name_s))?;
      let func = scope.alloc_native_function(call_id, None, name_s, 0)?;
      scope.push_root(Value::Object(func))?;

      let key = alloc_key(&mut scope, "__check_hooks_payload")?;
      scope.define_property(global, key, data_desc(Value::Object(func)))?;
    }

    window.exec_script("__check_hooks_payload()")?;
    Ok(())
  }

  #[test]
  fn window_realm_perform_microtask_checkpoint_runs_promise_jobs() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    // `WindowRealm` does not install the full HTML event loop / `queueMicrotask` bindings by
    // default, but Promise jobs still enqueue microtasks into the VM-owned microtask queue.
    realm
      .exec_script("globalThis.__x = 0; Promise.resolve().then(() => { globalThis.__x = 1; });")?;
    assert_eq!(realm.exec_script("globalThis.__x")?, Value::Number(0.0));

    realm.perform_microtask_checkpoint()?;
    assert_eq!(realm.exec_script("globalThis.__x")?, Value::Number(1.0));
    Ok(())
  }

  #[test]
  fn window_env_shims_exist_and_match_media_evaluates() -> Result<(), VmError> {
    let media = MediaContext::screen(800.0, 600.0).with_device_pixel_ratio(2.0);
    let mut realm =
      new_realm(WindowRealmConfig::new("https://example.com/").with_media_context(media))?;

    let dpr = realm.exec_script("devicePixelRatio")?;
    assert!(matches!(dpr, Value::Number(v) if (v - 2.0).abs() < f64::EPSILON));

    let ua = realm.exec_script("navigator.userAgent")?;
    assert_eq!(get_string(realm.heap(), ua), FASTRENDER_USER_AGENT);
    let platform = realm.exec_script("navigator.platform")?;
    assert_eq!(get_string(realm.heap(), platform), "Win32");

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
  fn window_crypto_exists_and_is_spec_shaped() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    assert_eq!(
      realm.exec_script(
        "(() => {\n\
          const ok1 = typeof crypto === 'object';\n\
          const ok2 = typeof Crypto === 'function';\n\
          const ok3 = crypto instanceof Crypto;\n\
          let illegal = false;\n\
          try { new Crypto(); } catch (e) {\n\
            illegal = e && e.name === 'TypeError' && String(e.message).includes('Illegal constructor');\n\
          }\n\
          return ok1 && ok2 && ok3 && illegal;\n\
        })()",
      )?,
      Value::Bool(true)
    );
    Ok(())
  }

  #[test]
  fn window_crypto_get_random_values_fills_bytes_and_is_stateful() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    assert_eq!(
      realm.exec_script(
        "(() => {\n\
          const a = new Uint8Array(16);\n\
          crypto.getRandomValues(a);\n\
          let allZero = true;\n\
          for (var i = 0; i < a.length; i++) { if (a[i] !== 0) { allZero = false; break; } }\n\
          const b = new Uint8Array(16);\n\
          crypto.getRandomValues(b);\n\
          let same = true;\n\
          for (var i = 0; i < a.length; i++) { if (a[i] !== b[i]) { same = false; break; } }\n\
          return !allZero && !same;\n\
        })()",
      )?,
      Value::Bool(true)
    );
    Ok(())
  }

  #[test]
  fn window_crypto_get_random_values_enforces_quota() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let name = realm.exec_script(
      "(() => {\n\
        try {\n\
          crypto.getRandomValues(new Uint8Array(65537));\n\
          return 'no error';\n\
        } catch (e) {\n\
          return e && e.name;\n\
        }\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), name), "QuotaExceededError");
    Ok(())
  }

  #[test]
  fn window_crypto_random_uuid_is_rfc4122_v4() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    assert_eq!(
      realm.exec_script(
        "(() => {\n\
          var a = crypto.randomUUID();\n\
          var b = crypto.randomUUID();\n\
          function ok(u) {\n\
            if (u.length !== 36) return false;\n\
            if (u.charCodeAt(8) !== 45 || u.charCodeAt(13) !== 45 || u.charCodeAt(18) !== 45 || u.charCodeAt(23) !== 45) return false;\n\
            for (var i = 0; i < u.length; i++) {\n\
              var c = u.charCodeAt(i);\n\
              if (i === 8 || i === 13 || i === 18 || i === 23) {\n\
                if (c !== 45) return false;\n\
                continue;\n\
              }\n\
              var isDigit = c >= 48 && c <= 57;\n\
              var isLowerHex = c >= 97 && c <= 102;\n\
              if (!(isDigit || isLowerHex)) return false;\n\
            }\n\
            if (u.charCodeAt(14) !== 52) return false;\n\
            var v = u.charCodeAt(19);\n\
            if (!(v === 56 || v === 57 || v === 97 || v === 98)) return false;\n\
            return true;\n\
          }\n\
          return ok(a) && ok(b) && a !== b;\n\
        })()",
      )?,
      Value::Bool(true)
    );
    Ok(())
  }

  #[test]
  fn window_storage_exists_and_round_trips() -> Result<(), VmError> {
    crate::js::web_storage::reset_default_web_storage_hub_for_tests();
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
  fn window_storage_set_item_enforces_quota() -> Result<(), VmError> {
    let mut realm = new_realm(
      WindowRealmConfig::new("https://example.com/").with_web_storage_quota_utf16_bytes(20),
    )?;

    let name = realm.exec_script(
      "(() => {\n\
        try {\n\
          localStorage.setItem('k', 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx');\n\
          return 'no error';\n\
        } catch (e) {\n\
          return e && e.name;\n\
        }\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), name), "QuotaExceededError");
    assert_eq!(realm.exec_script("localStorage.getItem('k')")?, Value::Null);
    assert!(matches!(
      realm.exec_script("localStorage.length")?,
      Value::Number(n) if n == 0.0
    ));
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
    assert_eq!(
      get_string(realm.heap(), href),
      "https://example.com/dir/file"
    );

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
    assert_eq!(
      realm.exec_script("typeof Date === 'function'")?,
      Value::Bool(true)
    );
    assert_eq!(
      realm.exec_script("typeof Date.now === 'function'")?,
      Value::Bool(true)
    );
    assert_eq!(
      realm.exec_script("new Date(123).getTime()")?,
      Value::Number(123.0)
    );
    assert_eq!(
      realm.exec_script("new Date().getTime()")?,
      Value::Number(1_000.0)
    );
    assert_eq!(
      realm.exec_script("performance.timeOrigin")?,
      Value::Number(web_time.time_origin_unix_ms as f64)
    );
    assert_eq!(realm.exec_script("Date.now()")?, Value::Number(1_000.0));
    assert_eq!(realm.exec_script("performance.now()")?, Value::Number(0.0));

    // Advance to a deterministic non-integer millisecond.
    clock.set_now(Duration::from_nanos(1_234_567_890)); // 1234.56789ms
    assert_eq!(realm.exec_script("Date.now()")?, Value::Number(2_234.0));
    assert_eq!(
      realm.exec_script("new Date().getTime()")?,
      Value::Number(2_234.0)
    );
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
    assert_eq!(get_string(realm.heap(), ready_state), "loading");

    let visibility = realm.exec_script("document.visibilityState")?;
    assert_eq!(get_string(realm.heap(), visibility), "visible");

    assert_eq!(realm.exec_script("document.hidden")?, Value::Bool(false));

    Ok(())
  }

  #[test]
  fn document_ready_state_reflects_dom2_document_ready_state() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    for (state, expected) in [
      (crate::web::dom::DocumentReadyState::Loading, "loading"),
      (
        crate::web::dom::DocumentReadyState::Interactive,
        "interactive",
      ),
      (crate::web::dom::DocumentReadyState::Complete, "complete"),
    ] {
      host.dom_mut().set_ready_state(state);
      let ready_state = exec_script_with_dom_host(&mut realm, &mut host, "document.readyState")?;
      assert_eq!(get_string(realm.heap(), ready_state), expected);
    }

    host
      .dom_mut()
      .set_ready_state(crate::web::dom::DocumentReadyState::Loading);
    exec_script_with_dom_host(&mut realm, &mut host, "document.readyState = 'complete'")?;
    let ready_state = exec_script_with_dom_host(&mut realm, &mut host, "document.readyState")?;
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
  fn event_constructors_require_new_and_expose_phase_constants() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let err = realm.exec_script("Event('x')");
    let err = err.expect_err("expected Event('x') to throw");
    match err {
      VmError::TypeError(msg) => assert_eq!(msg, "Event constructor cannot be invoked without 'new'"),
      other => {
        let obj = unwrap_thrown_object(other);
        let (vm, heap) = realm.vm_and_heap_mut();
        let mut scope = heap.scope();
        scope.push_root(Value::Object(obj))?;
        let name = get_prop(vm, &mut scope, obj, "name")?;
        assert_eq!(get_string(scope.heap(), name), "TypeError");
        let message = get_prop(vm, &mut scope, obj, "message")?;
        assert_eq!(
          get_string(scope.heap(), message),
          "Event constructor cannot be invoked without 'new'"
        );
      }
    }

    assert!(matches!(realm.exec_script("new Event('x')")?, Value::Object(_)));

    let err = realm.exec_script("CustomEvent('x')");
    let err = err.expect_err("expected CustomEvent('x') to throw");
    match err {
      VmError::TypeError(msg) => {
        assert_eq!(msg, "CustomEvent constructor cannot be invoked without 'new'")
      }
      other => {
        let obj = unwrap_thrown_object(other);
        let (vm, heap) = realm.vm_and_heap_mut();
        let mut scope = heap.scope();
        scope.push_root(Value::Object(obj))?;
        let name = get_prop(vm, &mut scope, obj, "name")?;
        assert_eq!(get_string(scope.heap(), name), "TypeError");
        let message = get_prop(vm, &mut scope, obj, "message")?;
        assert_eq!(
          get_string(scope.heap(), message),
          "CustomEvent constructor cannot be invoked without 'new'"
        );
      }
    }

    assert!(matches!(
      realm.exec_script("new CustomEvent('x')")?,
      Value::Object(_)
    ));

    // DOM constants should be present and numeric.
    assert_eq!(realm.exec_script("Event.NONE")?, Value::Number(0.0));
    assert_eq!(realm.exec_script("Event.CAPTURING_PHASE")?, Value::Number(1.0));
    assert_eq!(realm.exec_script("Event.AT_TARGET")?, Value::Number(2.0));
    assert_eq!(realm.exec_script("Event.BUBBLING_PHASE")?, Value::Number(3.0));

    // Constants are mirrored onto the interface prototype in browsers / WebIDL.
    assert_eq!(realm.exec_script("Event.prototype.CAPTURING_PHASE")?, Value::Number(1.0));

    // Constants are non-writable and non-configurable.
    assert_eq!(
      realm.exec_script(
        "(() => {\n\
          let threw = false;\n\
          try {\n\
            (function () { 'use strict'; Event.CAPTURING_PHASE = 9; })();\n\
          } catch (e) {\n\
            threw = true;\n\
          }\n\
          return threw || Event.CAPTURING_PHASE === 1;\n\
        })()",
      )?,
      Value::Bool(true)
    );
    assert_eq!(realm.exec_script("delete Event.CAPTURING_PHASE")?, Value::Bool(false));
    Ok(())
  }

  #[test]
  fn domless_window_and_document_event_targets_dispatch_via_fallback_registry(
  ) -> Result<(), VmError> {
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
  fn dom_wrappers_are_instanceof_event_target() -> Result<(), VmError> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "(() => {\n\
        const ok1 = document instanceof EventTarget;\n\
        const ok2 = document.body instanceof EventTarget;\n\
        const ok3 = document.createElement('div') instanceof EventTarget;\n\
        const ok4 = new EventTarget() instanceof EventTarget;\n\
        return ok1 && ok2 && ok3 && ok4;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let reaches_proto = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "(() => {\n\
        const el = document.createElement('div');\n\
        let p = Object.getPrototypeOf(el);\n\
        while (p) {\n\
          if (p === EventTarget.prototype) return true;\n\
          p = Object.getPrototypeOf(p);\n\
        }\n\
        return false;\n\
      })()",
    )?;
    assert_eq!(reaches_proto, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn dom_event_listeners_are_registered_in_dom2_and_invoked_by_host_dispatch() -> Result<(), VmError>
  {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
      let document_listener_roots =
        super::get_or_create_event_listener_roots(&mut scope, document_obj)?;
      let dom_ptr = NonNull::from(host.dom_mut());

      let mut vm_host = ();
      let mut hooks = NoopHostHooks::default();
      let mut invoker = super::VmJsDomEventInvoker {
        vm: &mut *vm,
        scope: &mut scope,
        vm_host: (&mut vm_host as &mut dyn VmHost) as *mut dyn VmHost,
        hooks: &mut hooks,
        window_obj: global,
        document_obj,
        event_obj,
        dom_ptr,
        document_listener_roots,
        opaque_target_obj: None,
        registry: host.dom().events() as *const _,
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
        host.dom(),
        host.dom().events(),
        &mut invoker,
      )
      .expect("dispatch_event should succeed")
    };

    assert_eq!(default_not_prevented, true);
    assert_eq!(realm.exec_script("__count")?, Value::Number(1.0));
    Ok(())
  }

  #[test]
  fn host_dom_event_dispatch_respects_max_instruction_count() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut js_execution_options = JsExecutionOptions::default();
    // Increase the wall time budget so debug builds running in parallel don't trip the default
    // budget.
    js_execution_options.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(2));
    // Keep the heap limits configured by `WindowRealmConfig`.
    js_execution_options.max_vm_heap_bytes = None;
    // Provide enough fuel for the initial script to install listeners.
    js_execution_options.max_instruction_count = Some(10_000);
    let mut realm = WindowRealm::new_with_js_execution_options(
      WindowRealmConfig::new("https://example.com/"),
      js_execution_options,
    )?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "globalThis.__ran = false;\n\
       document.addEventListener('x', () => { globalThis.__ran = true; });",
    )?;

    // Simulate stale VM state that would otherwise allow the listener to run.
    realm.js_execution_options.max_instruction_count = Some(0);
    realm.vm_mut().set_budget(vm_js::Budget::unlimited(100));

    struct DummyHost;
    impl WindowRealmHost for DummyHost {
      fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm) {
        unreachable!("DummyHost is only used as a type parameter for VmJsEventLoopHooks");
      }
    }

    let mut realm_slot = Some(realm);
    let mut vm_host_ctx = ();
    let mut vm_host_slot: Option<NonNull<dyn VmHost>> =
      Some(NonNull::from(&mut vm_host_ctx as &mut dyn VmHost));
    let mut webidl_bindings_host_slot: Option<NonNull<dyn WebIdlBindingsHost>> = None;
    let mut invoker = WindowRealmDomEventListenerInvoker::<DummyHost>::new(
      &mut realm_slot,
      &mut vm_host_slot,
      &mut webidl_bindings_host_slot,
    );

    let mut event = web_events::Event::new(
      "x",
      web_events::EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    let err = web_events::dispatch_event(
      web_events::EventTargetId::Document,
      &mut event,
      host.dom(),
      host.dom().events(),
      &mut invoker,
    )
    .expect_err("expected host-driven dispatch to abort when max_instruction_count is exhausted");

    assert!(
      err.to_string().to_lowercase().contains("fuel"),
      "expected out-of-fuel error, got: {err}"
    );

    // Inspect global state without `exec_script` (which would itself hit the zero fuel budget).
    let realm = realm_slot.as_mut().expect("expected realm slot");
    let realm_id = realm.realm_id;
    let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
    vm.set_budget(vm_js::Budget::unlimited(100));
    let mut scope = heap.scope();
    let mut vm = vm.execution_context_guard(vm_js::ExecutionContext {
      realm: realm_id,
      script_or_module: None,
    });
    let global = realm_ref.global_object();
    assert_eq!(
      get_prop(&mut vm, &mut scope, global, "__ran")?,
      Value::Bool(false)
    );

    Ok(())
  }

  #[test]
  fn host_dispatched_storage_event_exposes_storage_event_fields() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;
    exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "globalThis.__has_storage_event_ctor = (typeof StorageEvent === 'function');\n\
       globalThis.__storage_events = [];\n\
       addEventListener('storage', (e) => {\n\
         __storage_events.push({\n\
           key: e.key,\n\
           oldValue: e.oldValue,\n\
           newValue: e.newValue,\n\
           url: e.url,\n\
           area: (e.storageArea === localStorage)\n\
             ? 'local'\n\
             : ((e.storageArea === sessionStorage) ? 'session' : 'other'),\n\
           isInstance: __has_storage_event_ctor ? (e instanceof StorageEvent) : null,\n\
         });\n\
       });",
    )?;

    struct DummyHost;
    impl WindowRealmHost for DummyHost {
      fn vm_host_and_window_realm(&mut self) -> (&mut dyn VmHost, &mut WindowRealm) {
        unreachable!("DummyHost is only used as a type parameter for VmJsEventLoopHooks");
      }
    }

    let mut realm_slot = Some(realm);
    let mut vm_host_ctx = ();
    let mut vm_host_slot: Option<NonNull<dyn VmHost>> =
      Some(NonNull::from(&mut vm_host_ctx as &mut dyn VmHost));
    let mut webidl_bindings_host_slot: Option<NonNull<dyn WebIdlBindingsHost>> = None;
    let mut invoker = WindowRealmDomEventListenerInvoker::<DummyHost>::new(
      &mut realm_slot,
      &mut vm_host_slot,
      &mut webidl_bindings_host_slot,
    );

    let mut set_item_event = web_events::Event::new(
      "storage",
      web_events::EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    set_item_event.storage = Some(web_events::StorageEventData {
      key: Some("a".into()),
      old_value: Some("1".into()),
      new_value: Some("2".into()),
      url: "https://example.com/".into(),
      storage_kind: web_events::StorageKind::Local,
    });
    web_events::dispatch_event(
      web_events::EventTargetId::Window,
      &mut set_item_event,
      host.dom(),
      host.dom().events(),
      &mut invoker,
    )
    .expect("dispatch_event should succeed");

    let mut remove_item_event = web_events::Event::new(
      "storage",
      web_events::EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    remove_item_event.storage = Some(web_events::StorageEventData {
      key: Some("b".into()),
      old_value: Some("x".into()),
      new_value: None,
      url: "https://example.com/".into(),
      storage_kind: web_events::StorageKind::Session,
    });
    web_events::dispatch_event(
      web_events::EventTargetId::Window,
      &mut remove_item_event,
      host.dom(),
      host.dom().events(),
      &mut invoker,
    )
    .expect("dispatch_event should succeed");

    let mut clear_event = web_events::Event::new(
      "storage",
      web_events::EventInit {
        bubbles: false,
        cancelable: false,
        composed: false,
      },
    );
    clear_event.storage = Some(web_events::StorageEventData {
      key: None,
      old_value: None,
      new_value: None,
      url: "https://example.com/".into(),
      storage_kind: web_events::StorageKind::Local,
    });
    web_events::dispatch_event(
      web_events::EventTargetId::Window,
      &mut clear_event,
      host.dom(),
      host.dom().events(),
      &mut invoker,
    )
    .expect("dispatch_event should succeed");

    let realm = realm_slot.as_mut().expect("expected realm slot");
    let has_ctor = match realm.exec_script("__has_storage_event_ctor")? {
      Value::Bool(v) => v,
      other => panic!("expected boolean, got {other:?}"),
    };
    let is_instance = if has_ctor {
      serde_json::Value::Bool(true)
    } else {
      serde_json::Value::Null
    };

    let got_v = realm.exec_script("JSON.stringify(__storage_events)")?;
    let got = get_string(realm.heap(), got_v);
    let got: serde_json::Value = serde_json::from_str(&got).expect("expected valid JSON");

    assert_eq!(
      got,
      serde_json::json!([
        {
          "key": "a",
          "oldValue": "1",
          "newValue": "2",
          "url": "https://example.com/",
          "area": "local",
          "isInstance": is_instance.clone(),
        },
        {
          "key": "b",
          "oldValue": "x",
          "newValue": null,
          "url": "https://example.com/",
          "area": "session",
          "isInstance": is_instance.clone(),
        },
        {
          "key": null,
          "oldValue": null,
          "newValue": null,
          "url": "https://example.com/",
          "area": "local",
          "isInstance": is_instance.clone(),
        }
      ])
    );

    Ok(())
  }

  #[test]
  fn dom_event_once_listeners_are_removed_after_first_dispatch() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
      let document_listener_roots =
        super::get_or_create_event_listener_roots(&mut scope, document_obj)?;
      let dom_ptr = NonNull::from(host.dom_mut());
      let mut vm_host = ();
      let mut hooks = NoopHostHooks::default();
      let mut invoker = super::VmJsDomEventInvoker {
        vm: &mut *vm,
        scope: &mut scope,
        vm_host: (&mut vm_host as &mut dyn VmHost) as *mut dyn VmHost,
        hooks: &mut hooks,
        window_obj: global,
        document_obj,
        event_obj,
        dom_ptr,
        document_listener_roots,
        opaque_target_obj: None,
        registry: host.dom().events() as *const _,
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
        host.dom(),
        host.dom().events(),
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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
      let document_listener_roots =
        super::get_or_create_event_listener_roots(&mut scope, document_obj)?;
      let dom_ptr = NonNull::from(host.dom_mut());
      let mut vm_host = ();
      let mut hooks = NoopHostHooks::default();
      let mut invoker = super::VmJsDomEventInvoker {
        vm: &mut *vm,
        scope: &mut scope,
        vm_host: (&mut vm_host as &mut dyn VmHost) as *mut dyn VmHost,
        hooks: &mut hooks,
        window_obj: global,
        document_obj,
        event_obj,
        dom_ptr,
        document_listener_roots,
        opaque_target_obj: None,
        registry: host.dom().events() as *const _,
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
        host.dom(),
        host.dom().events(),
        &mut invoker,
      )
      .expect("dispatch_event should succeed");
    }
    assert_eq!(realm.exec_script("__count")?, Value::Number(1.0));
    Ok(())
  }

  #[test]
  fn event_target_listeners_can_be_registered_and_dispatched() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

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
  fn storage_event_constructor_and_document_create_event_exist() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    assert_eq!(
      realm.exec_script("typeof StorageEvent === 'function'")?,
      Value::Bool(true)
    );

    let ty = realm.exec_script("new StorageEvent('storage').type")?;
    assert_eq!(get_string(realm.heap(), ty), "storage");

    assert_eq!(
      realm.exec_script("typeof document.createEvent('StorageEvent').initStorageEvent === 'function'")?,
      Value::Bool(true)
    );
    Ok(())
  }

  #[test]
  fn event_target_constructor_exists_and_dispatches() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

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
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

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
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

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
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
    let document_listener_roots =
      super::get_or_create_event_listener_roots(&mut scope, document_obj)?;
    let dom_ptr = NonNull::from(host.dom_mut());
    let vm_host = (&mut host as &mut dyn VmHost) as *mut dyn VmHost;
    let mut hooks = NoopHostHooks::default();
    let mut invoker = super::VmJsDomEventInvoker {
      vm: &mut *vm,
      scope: &mut scope,
      vm_host,
      hooks: &mut hooks,
      window_obj: global,
      document_obj,
      event_obj,
      dom_ptr,
      document_listener_roots,
      opaque_target_obj: None,
      registry: host.dom().events() as *const _,
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
      host.dom(),
      host.dom().events(),
      &mut invoker,
    )
    .expect("dispatch_event should succeed");
    assert_eq!(default_not_prevented, false);
    assert_eq!(event.default_prevented, true);
    Ok(())
  }

  #[test]
  fn dom_event_prevent_default_does_not_affect_other_event_objects() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "document.addEventListener('x', (_e) => {\n\
         const other = new Event('x', { cancelable: true });\n\
         other.preventDefault();\n\
       });\n\
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
    let document_listener_roots =
      super::get_or_create_event_listener_roots(&mut scope, document_obj)?;
    let dom_ptr = NonNull::from(host.dom_mut());
    let vm_host = (&mut host as &mut dyn VmHost) as *mut dyn VmHost;
    let mut hooks = NoopHostHooks::default();
    let mut invoker = super::VmJsDomEventInvoker {
      vm: &mut *vm,
      scope: &mut scope,
      vm_host,
      hooks: &mut hooks,
      window_obj: global,
      document_obj,
      event_obj,
      dom_ptr,
      document_listener_roots,
      opaque_target_obj: None,
      registry: host.dom().events() as *const _,
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
      host.dom(),
      host.dom().events(),
      &mut invoker,
    )
    .expect("dispatch_event should succeed");
    assert_eq!(default_not_prevented, true);
    assert_eq!(event.default_prevented, false);
    Ok(())
  }

  #[test]
  fn node_wrappers_expose_event_target_methods() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let called = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;
    exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "document.documentElement.className = 'hello'",
    )?;

    let doc_el = host
      .dom()
      .document_element()
      .expect("document element should exist");
    assert_eq!(host.dom().element_class_name(doc_el), "hello");
    Ok(())
  }

  #[test]
  fn element_class_list_mutates_dom2_document() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=target class=\"a b\"></div></body></html>",
    )
    .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let target = host
      .dom()
      .get_element_by_id("target")
      .expect("missing #target");
    assert_eq!(host.dom().element_class_name(target), "f c d e");
    Ok(())
  }

  #[test]
  fn element_dataset_shim_mutates_dom2_document() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=target data-foo-bar=\"baz\"></div></body></html>",
    )
    .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "(() => {\n\
        const el = document.getElementById('target');\n\
        if (!el) return false;\n\
        if (el.dataset.fooBar !== 'baz') return false;\n\
        el.dataset.fooBar = 'qux';\n\
        if (el.getAttribute('data-foo-bar') !== 'qux') return false;\n\
        if (el.dataset.fooBar !== 'qux') return false;\n\
        delete el.dataset.fooBar;\n\
        if (el.getAttribute('data-foo-bar') !== null) return false;\n\
        // Invalid property names should not throw and should not create attributes.\n\
        try { el.dataset.Foo = 'x'; } catch (e) { return false; }\n\
        try { el.dataset['foo-bar'] = 'y'; } catch (e) { return false; }\n\
        return el.getAttribute('data-foo') === null;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));

    let target = host
      .dom()
      .get_element_by_id("target")
      .expect("missing #target");
    assert_eq!(
      host.dom().get_attribute(target, "data-foo-bar").unwrap(),
      None
    );
    Ok(())
  }

  #[test]
  fn element_reflected_attributes_mutate_dom2_document() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let script = host.dom().get_element_by_id("s").expect("missing #s");
    assert_eq!(
      host.dom().get_attribute(script, "src").unwrap(),
      Some("https://example.com/app.js")
    );
    assert_eq!(
      host.dom().get_attribute(script, "type").unwrap(),
      Some("module")
    );
    assert_eq!(
      host.dom().get_attribute(script, "charset").unwrap(),
      Some("utf-8")
    );
    assert_eq!(
      host.dom().get_attribute(script, "crossorigin").unwrap(),
      Some("anonymous")
    );
    assert_eq!(host.dom().has_attribute(script, "async").unwrap(), true);
    assert_eq!(host.dom().has_attribute(script, "defer").unwrap(), true);

    Ok(())
  }

  #[test]
  fn html_script_element_async_reflects_force_async_slot() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><script id=s></script></body></html>")
        .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let div = host.dom().get_element_by_id("t").expect("missing #t");
    assert_eq!(
      host.dom().get_attribute(div, "style").unwrap(),
      Some("background-color: red; cursor: pointer; height: 10px;")
    );

    Ok(())
  }

  #[test]
  fn dom_shims_mutate_host_dom() -> Result<(), VmError> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "(() => {\n\
        const el = document.getElementById('target');\n\
        el.dataset.x = 'y';\n\
        el.classList.add('a');\n\
        el.style.setProperty('color', 'red');\n\
      })()",
    )?;

    let target = host
      .dom()
      .get_element_by_id("target")
      .expect("missing #target");
    assert_eq!(
      host.dom().get_attribute(target, "data-x").unwrap(),
      Some("y")
    );
    assert_eq!(host.dom().element_class_name(target), "a");
    assert_eq!(host.dom().style_get_property_value(target, "color"), "red");
    Ok(())
  }

  #[test]
  fn element_inner_html_round_trips_via_window_realm_shim() -> Result<(), VmError> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=target></div></body></html>")
        .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "document.getElementById('target').innerHTML = '<span>hi</span>'",
    )?;
    let inner = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "document.getElementById('target').innerHTML",
    )?;
    assert_eq!(get_string(realm.heap(), inner), "<span>hi</span>");

    let target = host
      .dom()
      .get_element_by_id("target")
      .expect("missing #target");
    assert_eq!(host.dom().inner_html(target).unwrap(), "<span>hi</span>");
    Ok(())
  }

  #[test]
  fn element_outer_html_setter_replaces_node_in_dom2_via_window_realm_shim() -> Result<(), VmError>
  {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=child>hi</span></div></body></html>",
    )
    .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "document.getElementById('child').outerHTML = '<p>one</p><p>two</p>'",
    )?;

    let root = host.dom().get_element_by_id("root").expect("missing #root");
    assert_eq!(host.dom().inner_html(root).unwrap(), "<p>one</p><p>two</p>");

    let outer = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "document.getElementById('root').outerHTML",
    )?;
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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let result = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "document.getElementById('target').insertAdjacentHTML('beforeend', '<b>hi</b>')",
    )?;
    assert_eq!(result, Value::Undefined);

    let target = host
      .dom()
      .get_element_by_id("target")
      .expect("missing #target");
    assert_eq!(host.dom().inner_html(target).unwrap(), "<b>hi</b>");

    let err_name = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "document.getElementById('target').insertAdjacentText('afterbegin', 'x')",
    )?;

    let target = host
      .dom()
      .get_element_by_id("target")
      .expect("missing #target");
    assert_eq!(host.dom().inner_html(target).unwrap(), "x");
    Ok(())
  }

  #[test]
  fn element_insert_adjacent_element_inserts_and_returns_element() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=target></span></div></body></html>",
    )
    .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let inserted_is_same = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "(() => {\n\
        const b = document.createElement('b');\n\
        b.appendChild(document.createElement('i'));\n\
        const target = document.getElementById('target');\n\
        return target.insertAdjacentElement('beforebegin', b) === b;\n\
      })()",
    )?;
    assert_eq!(inserted_is_same, Value::Bool(true));

    let root = host.dom().get_element_by_id("root").expect("missing #root");
    assert_eq!(
      host.dom().inner_html(root).unwrap(),
      r#"<b><i></i></b><span id="target"></span>"#
    );

    Ok(())
  }

  #[test]
  fn document_create_text_node_and_node_text_content_round_trip() -> Result<(), VmError> {
    let renderer_dom =
      crate::dom::parse_html("<!doctype html><html><body><div id=root></div></body></html>")
        .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    // This test performs several DOM operations (including fragment parsing for `innerHTML`). Run
    // them across multiple script evaluations so each call stays within the per-run VM wall-time
    // budget.
    let step1 = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let step2 = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "(() => {\n\
        const root = document.getElementById('root');\n\
        root.innerHTML = '<span>hi</span><b>!</b>';\n\
        if (root.textContent !== 'hi!') return 'r3:' + root.textContent;\n\
        return 'ok';\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), step2), "ok");

    let step3 = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let step4 = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let step5 = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let step6 = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "(() => {\n\
        const docNode = document.documentElement.parentNode;\n\
        if (docNode.textContent !== null) return 'doc:' + docNode.textContent;\n\
        return 'ok';\n\
      })()",
    )?;
    assert_eq!(get_string(realm.heap(), step6), "ok");

    let root = host.dom().get_element_by_id("root").expect("missing #root");
    assert_eq!(host.dom().inner_html(root).unwrap(), "ab");

    Ok(())
  }

  #[test]
  fn node_remove_child_detaches_and_returns_child() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=child>hi</span><b id=other></b></div></body></html>",
    )
    .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let root = host.dom().get_element_by_id("root").expect("missing #root");
    assert_eq!(
      host.dom().inner_html(root).unwrap(),
      r#"<b id="other"></b>"#
    );

    Ok(())
  }

  #[test]
  fn node_insert_before_inserts_before_reference_or_appends_on_null() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=ref>hi</span></div></body></html>",
    )
    .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let root = host.dom().get_element_by_id("root").expect("missing #root");
    assert_eq!(
      host.dom().inner_html(root).unwrap(),
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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let root = host.dom().get_element_by_id("root").expect("missing #root");
    assert_eq!(host.dom().inner_html(root).unwrap(), r#"<p id="new"></p>"#);

    Ok(())
  }

  #[test]
  fn node_clone_node_clones_subtree_and_preserves_attributes() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=a><span>hello</span></div></body></html>",
    )
    .unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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

    let root = host.dom().get_element_by_id("root").expect("missing #root");
    assert_eq!(
      host.dom().inner_html(root).unwrap(),
      r#"<span id="b"></span>"#
    );

    Ok(())
  }

  #[test]
  fn node_child_nodes_is_live_and_cached_across_mutations() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
  fn document_methods_throw_type_error_on_illegal_invocation() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
      "(() => {\n\
        const bogus = document.createElement('div');\n\
        try { document.getElementById.call(bogus, 'x'); return false; }\n\
        catch (e) { if (e.name !== 'TypeError' || e.message !== 'Illegal invocation') return false; }\n\
        try { document.querySelector.call(bogus, 'body'); return false; }\n\
        catch (e) { if (e.name !== 'TypeError' || e.message !== 'Illegal invocation') return false; }\n\
        try { document.querySelectorAll.call(bogus, 'body'); return false; }\n\
        catch (e) { if (e.name !== 'TypeError' || e.message !== 'Illegal invocation') return false; }\n\
        return true;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn text_node_basics_instanceof_owner_document_and_is_connected() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html><body></body></html>").unwrap();
    let mut host = crate::js::HostDocumentState::from_renderer_dom(&renderer_dom);

    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = exec_script_with_dom_host(
      &mut realm,
      &mut host,
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
      let mut config =
        WindowRealmConfig::new("https://example.com/").with_js_execution_options(js_options);
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
      captured_for_sink
        .lock()
        .push(CapturedConsoleCall { level, args });
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
      ("trace", ConsoleMessageLevel::Debug, Value::Number(6.0)),
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
        CapturedConsoleCall {
          level: ConsoleMessageLevel::Debug,
          args: vec![CapturedConsoleArg::Number(6.0)]
        },
      ]
    );
    Ok(())
  }

  #[test]
  fn console_extra_methods_exist_and_do_not_throw() -> Result<(), VmError> {
    let mut realm = new_realm(WindowRealmConfig::new("https://example.com/"))?;

    let ok = realm.exec_script(
      "(() => {\n\
        const c = console;\n\
        if (c.trace(undefined, null, true, 1, 's', {}, Symbol('x')) !== undefined) return false;\n\
        if (c.group('label') !== undefined) return false;\n\
        if (c.groupCollapsed('label') !== undefined) return false;\n\
        if (c.groupEnd() !== undefined) return false;\n\
        if (c.clear() !== undefined) return false;\n\
        // Methods must remain callable even when extracted (no `this` binding).\n\
        const { trace, group, groupCollapsed, groupEnd, clear } = c;\n\
        trace('x'); group('x'); groupCollapsed('x'); groupEnd(); clear();\n\
        return true;\n\
      })()",
    )?;
    assert_eq!(ok, Value::Bool(true));
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
