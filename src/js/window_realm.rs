use crate::dom2::{self, NodeId};
use crate::js::cookie_jar::{CookieJar, MAX_COOKIE_STRING_BYTES};
use crate::js::CurrentScriptStateHandle;
use crate::js::window_env::{install_window_shims_vm_js, unregister_match_media_env, MatchMediaEnvGuard, WindowEnv};
use crate::style::media::MediaContext;
use crate::resource::ResourceFetcher;
use base64::engine::general_purpose;
use base64::Engine as _;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use url::Url;
use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime as VmJsRuntime, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, RealmId, Scope, SourceText, Value, Vm, VmError, VmHost, VmHostHooks,
  VmOptions,
};

pub type ConsoleSink = Arc<dyn Fn(&vm_js::Heap, &[vm_js::Value]) + Send + Sync + 'static>;

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
}

impl WindowRealmConfig {
  pub fn new(document_url: impl Into<String>) -> Self {
    Self {
      document_url: document_url.into(),
      media: MediaContext::screen(800.0, 600.0),
      dom_source_id: None,
      current_script_state: None,
      console_sink: None,
      heap_limits: default_heap_limits(),
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

  pub fn with_heap_limits(mut self, limits: HeapLimits) -> Self {
    self.heap_limits = limits;
    self
  }
}

pub struct WindowRealm {
  runtime: VmJsRuntime,
  realm_id: RealmId,
  console_sink_id: Option<u64>,
  current_script_source_id: Option<u64>,
  match_media_env_id: Option<u64>,
}

struct WindowRealmUserData {
  document_url: String,
  cookie_fetcher: Option<Arc<dyn ResourceFetcher>>,
  cookie_jar: CookieJar,
}

impl std::fmt::Debug for WindowRealmUserData {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WindowRealmUserData")
      .field("document_url", &self.document_url)
      .field("has_cookie_fetcher", &self.cookie_fetcher.is_some())
      .field("cookie_jar", &self.cookie_jar)
      .finish()
  }
}

impl WindowRealmUserData {
  fn new(document_url: String) -> Self {
    Self {
      document_url,
      cookie_fetcher: None,
      cookie_jar: CookieJar::new(),
    }
  }
}

impl WindowRealm {
  pub fn new(config: WindowRealmConfig) -> Result<Self, VmError> {
    let mut vm_options = VmOptions::default();
    // Window realms should be interruptible even before full script execution is wired up.
    // This is separate from the renderer-level interrupt flag; callers can wire it up as needed.
    vm_options.interrupt_flag = Some(Arc::new(AtomicBool::new(false)));
    let vm = Vm::new(vm_options);
    let heap = Heap::new(config.heap_limits);

    let mut runtime = VmJsRuntime::new(vm, heap)?;
    runtime
      .vm
      .set_user_data(WindowRealmUserData::new(config.document_url.clone()));
    let realm_id = runtime.realm().id();

    // `vm-js::JsRuntime` does not expose a borrow-splitting accessor for `(vm, realm, heap)`. Use a
    // raw pointer to the realm to allow simultaneously borrowing `vm`/`heap` mutably.
    //
    // SAFETY: `vm-js::JsRuntime` stores `vm`, `heap`, and `realm` as disjoint fields. We do not
    // move the runtime while these borrows are live.
    let realm_ptr = runtime.realm() as *const Realm;
    let (vm, heap) = (&mut runtime.vm, &mut runtime.heap);
    let realm = unsafe { &*realm_ptr };

    let (console_sink_id, current_script_source_id, match_media_env_id) =
      init_window_globals(vm, heap, realm, &config)?;
    Ok(Self {
      runtime,
      realm_id,
      console_sink_id,
      current_script_source_id,
      match_media_env_id,
    })
  }

  pub fn reset_interrupt(&self) {
    self.runtime.vm.reset_interrupt();
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
    (&mut self.runtime.vm, &mut self.runtime.heap)
  }

  pub fn vm_realm_and_heap_mut(&mut self) -> (&mut Vm, &Realm, &mut Heap) {
    // SAFETY: `realm` is stored separately from `vm` and `heap` inside `vm-js::JsRuntime`.
    let realm_ptr = self.runtime.realm() as *const Realm;
    let vm = &mut self.runtime.vm;
    let heap = &mut self.runtime.heap;
    let realm = unsafe { &*realm_ptr };
    (vm, realm, heap)
  }

  pub fn realm(&self) -> &Realm {
    self.runtime.realm()
  }

  pub fn global_object(&self) -> GcObject {
    self.runtime.realm().global_object()
  }

  pub fn teardown(&mut self) {
    if let Some(id) = self.console_sink_id.take() {
      unregister_console_sink(id);
    }
    if let Some(id) = self.current_script_source_id.take() {
      unregister_current_script_source(id);
    }
    if let Some(id) = self.match_media_env_id.take() {
      unregister_match_media_env(id);
    }
    crate::js::window_url::teardown_window_url_bindings_for_realm(
      self.runtime.realm().id(),
      &mut self.runtime.heap,
    );
  }

  pub fn set_cookie_fetcher(&mut self, fetcher: Arc<dyn ResourceFetcher>) {
    if let Some(data) = self.runtime.vm.user_data_mut::<WindowRealmUserData>() {
      data.cookie_fetcher = Some(fetcher);
    }
  }

  /// Execute a classic script in this window realm.
  pub fn exec_script(&mut self, source: &str) -> Result<Value, VmError> {
    self.runtime.exec_script(source)
  }

  pub(crate) fn exec_script_with_host(
    &mut self,
    hooks: &mut dyn VmHostHooks,
    source: &str,
  ) -> Result<Value, VmError> {
    // `vm-js::JsRuntime::exec_script_with_hooks` routes Promise jobs through `VmHostHooks` instead
    // of the VM-owned microtask queue. `WindowHost` uses this to integrate Promise jobs into
    // FastRender's HTML-like microtask queue.
    self.runtime.exec_script_with_hooks(hooks, source)
  }

  /// Execute a classic script with an explicit source name for stack traces.
  pub fn exec_script_with_name(
    &mut self,
    source_name: impl Into<Arc<str>>,
    source_text: impl Into<Arc<str>>,
  ) -> Result<Value, VmError> {
    self
      .runtime
      .exec_script_source(Arc::new(SourceText::new(source_name, source_text)))
  }
}

pub trait WindowRealmHost {
  fn window_realm(&mut self) -> &mut WindowRealm;
}

impl Drop for WindowRealm {
  fn drop(&mut self) {
    self.teardown();
  }
}

impl crate::js::ecma_microtasks::VmJsEngineHost for WindowRealm {
  fn vm_js_heap(&self) -> &vm_js::Heap {
    self.heap()
  }

  fn vm_js_vm_and_heap_mut(&mut self) -> (&mut vm_js::Vm, &mut vm_js::Heap) {
    self.vm_and_heap_mut()
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
fn default_heap_limits() -> HeapLimits {
  const DEFAULT_HEAP_MAX_BYTES: usize = 32 * 1024 * 1024;
  const MIN_HEAP_MAX_BYTES: usize = 4 * 1024 * 1024;

  let mut max = DEFAULT_HEAP_MAX_BYTES;

  // If the process is constrained by `RLIMIT_AS` (typically applied by FastRender CLI flags or
  // an outer `prlimit`/cgroup), keep JS heap usage to a small fraction of that ceiling so other
  // renderer subsystems still have headroom.
  #[cfg(target_os = "linux")]
  {
    if let Ok((cur, _max)) = crate::process_limits::get_address_space_limit_bytes() {
      if cur > 0 && cur < u64::MAX {
        let suggested = cur / 8;
        if let Ok(suggested) = usize::try_from(suggested) {
          max = max.min(suggested.max(MIN_HEAP_MAX_BYTES));
        }
      }
    }
  }

  let gc_threshold = (max / 2).min(max);
  HeapLimits::new(max, gc_threshold)
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

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
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

const LOCATION_URL_KEY: &str = "__fastrender_location_url";
const CURRENT_SCRIPT_SOURCE_ID_KEY: &str = "__fastrender_current_script_source_id";
const NODE_WRAPPER_CACHE_KEY: &str = "__fastrender_node_wrapper_cache";
const NODE_ID_KEY: &str = "__fastrender_node_id";
const DOM_SOURCE_ID_KEY: &str = "__fastrender_dom_source_id";
const WRAPPER_DOCUMENT_KEY: &str = "__fastrender_wrapper_document";
const EVENT_PROTOTYPE_KEY: &str = "__fastrender_event_prototype";
const CUSTOM_EVENT_PROTOTYPE_KEY: &str = "__fastrender_custom_event_prototype";
const ELEMENT_CLASS_NAME_GET_KEY: &str = "__fastrender_element_class_name_get";
const ELEMENT_CLASS_NAME_SET_KEY: &str = "__fastrender_element_class_name_set";
const ELEMENT_ID_GET_KEY: &str = "__fastrender_element_id_get";
const ELEMENT_ID_SET_KEY: &str = "__fastrender_element_id_set";
const NODE_APPEND_CHILD_KEY: &str = "__fastrender_node_append_child";
const NODE_INSERT_BEFORE_KEY: &str = "__fastrender_node_insert_before";
const NODE_REMOVE_CHILD_KEY: &str = "__fastrender_node_remove_child";
const NODE_REPLACE_CHILD_KEY: &str = "__fastrender_node_replace_child";
const NODE_CLONE_NODE_KEY: &str = "__fastrender_node_clone_node";
const ELEMENT_GET_ATTRIBUTE_KEY: &str = "__fastrender_element_get_attribute";
const ELEMENT_SET_ATTRIBUTE_KEY: &str = "__fastrender_element_set_attribute";
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

thread_local! {
  static CURRENT_SCRIPT_SOURCES: RefCell<HashMap<u64, CurrentScriptStateHandle>> =
    RefCell::new(HashMap::new());
  static DOM_SOURCES: RefCell<HashMap<u64, NonNull<dom2::Document>>> =
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

fn console_log_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(console_obj) = this else {
    return Ok(Value::Undefined);
  };

  let key_s = scope.alloc_string("__fastrender_console_sink_id")?;
  let key = PropertyKey::from_string(key_s);
  let id = match scope
    .heap()
    .object_get_own_data_property_value(console_obj, &key)?
  {
    Some(Value::Number(n)) => n as u64,
    _ => return Ok(Value::Undefined),
  };

  let sink = console_sinks().lock().get(&id).cloned();
  if let Some(sink) = sink {
    sink(scope.heap(), args);
  }

  Ok(Value::Undefined)
}

const MAX_BASE64_INPUT_LEN: usize = 32 * 1024 * 1024;
const MAX_BASE64_OUTPUT_LEN: usize = 32 * 1024 * 1024;

fn window_report_error_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let value = args.get(0).copied().unwrap_or(Value::Undefined);

  // `reportError` must not throw. Avoid calling JS `ToString` since `Symbol` throws.
  let formatted = (|| -> Result<String, VmError> {
    Ok(match value {
      Value::Undefined => "undefined".to_string(),
      Value::Null => "null".to_string(),
      Value::Bool(b) => b.to_string(),
      Value::Number(n) => n.to_string(),
      Value::BigInt(b) => b.to_decimal_string(),
      Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
      Value::Symbol(_) => "[symbol]".to_string(),
      Value::Object(obj) => {
        // Best-effort: try to format `{name,message}` error-like shapes without invoking user code.
        let name_key = alloc_key(scope, "name")?;
        let message_key = alloc_key(scope, "message")?;

        let name = match scope.heap().object_get_own_data_property_value(obj, &name_key)? {
          Some(Value::String(s)) => scope.heap().get_string(s)?.to_utf8_lossy(),
          _ => String::new(),
        };
        let message = match scope.heap().object_get_own_data_property_value(obj, &message_key)? {
          Some(Value::String(s)) => scope.heap().get_string(s)?.to_utf8_lossy(),
          _ => String::new(),
        };

        if !name.is_empty() && !message.is_empty() {
          format!("{name}: {message}")
        } else if !name.is_empty() {
          name
        } else if !message.is_empty() {
          message
        } else {
          "[object]".to_string()
        }
      }
    })
  })()
  .unwrap_or_else(|_| "[reportError]".to_string());

  eprintln!("[js][reportError] {formatted}");
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
  if stripped.iter().copied().any(|b| !is_base64_alphabet_byte(b)) {
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

fn location_href_set_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "Navigation via location.href is not implemented yet",
  ))
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

fn parse_location_url(scope: &mut Scope<'_>, location_obj: GcObject) -> Result<Option<Url>, VmError> {
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
  Ok(Value::String(scope.alloc_string(url.host_str().unwrap_or(""))?))
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

fn get_or_create_node_wrapper(
  scope: &mut Scope<'_>,
  document_obj: GcObject,
  node_id: NodeId,
) -> Result<Value, VmError> {
  let cache_key = alloc_key(scope, NODE_WRAPPER_CACHE_KEY)?;
  let cache = match scope
    .heap()
    .object_get_own_data_property_value(document_obj, &cache_key)?
  {
    Some(Value::Object(obj)) => obj,
    _ => {
      let cache = scope.alloc_object()?;
      scope.push_root(Value::Object(cache))?;
      scope.define_property(
        document_obj,
        cache_key,
        data_desc(Value::Object(cache)),
      )?;
      cache
    }
  };

  let mut buf = [0u8; 20];
  let key_str = decimal_str_for_usize(node_id.index(), &mut buf);
  let wrapper_key = alloc_key(scope, key_str)?;

  if let Some(existing) = scope
    .heap()
    .object_get_own_data_property_value(cache, &wrapper_key)?
  {
    if let Value::Object(obj) = existing {
      return Ok(Value::Object(obj));
    }
  }

  let dom_source_id_key = alloc_key(scope, DOM_SOURCE_ID_KEY)?;
  let dom_source_id_value = scope
    .heap()
    .object_get_own_data_property_value(document_obj, &dom_source_id_key)?
    .unwrap_or(Value::Undefined);

  let element_query_selector = {
    let key = alloc_key(scope, ELEMENT_QUERY_SELECTOR_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let element_query_selector_all = {
    let key = alloc_key(scope, ELEMENT_QUERY_SELECTOR_ALL_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let element_matches = {
    let key = alloc_key(scope, ELEMENT_MATCHES_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let element_closest = {
    let key = alloc_key(scope, ELEMENT_CLOSEST_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };

  let class_name_get = {
    let key = alloc_key(scope, ELEMENT_CLASS_NAME_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let class_name_set = {
    let key = alloc_key(scope, ELEMENT_CLASS_NAME_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let id_get = {
    let key = alloc_key(scope, ELEMENT_ID_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let id_set = {
    let key = alloc_key(scope, ELEMENT_ID_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let append_child = {
    let key = alloc_key(scope, NODE_APPEND_CHILD_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let insert_before = {
    let key = alloc_key(scope, NODE_INSERT_BEFORE_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let remove_child = {
    let key = alloc_key(scope, NODE_REMOVE_CHILD_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let replace_child = {
    let key = alloc_key(scope, NODE_REPLACE_CHILD_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let clone_node = {
    let key = alloc_key(scope, NODE_CLONE_NODE_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let get_attribute = {
    let key = alloc_key(scope, ELEMENT_GET_ATTRIBUTE_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let set_attribute = {
    let key = alloc_key(scope, ELEMENT_SET_ATTRIBUTE_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let inner_html_get = {
    let key = alloc_key(scope, ELEMENT_INNER_HTML_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let inner_html_set = {
    let key = alloc_key(scope, ELEMENT_INNER_HTML_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let outer_html_get = {
    let key = alloc_key(scope, ELEMENT_OUTER_HTML_GET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let outer_html_set = {
    let key = alloc_key(scope, ELEMENT_OUTER_HTML_SET_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let insert_adjacent_html = {
    let key = alloc_key(scope, ELEMENT_INSERT_ADJACENT_HTML_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let insert_adjacent_element = {
    let key = alloc_key(scope, ELEMENT_INSERT_ADJACENT_ELEMENT_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let insert_adjacent_text = {
    let key = alloc_key(scope, ELEMENT_INSERT_ADJACENT_TEXT_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };

  let wrapper = scope.alloc_object()?;
  scope.push_root(Value::Object(wrapper))?;

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

  if let Some(Value::Object(func)) = get_attribute {
    let key = alloc_key(scope, "getAttribute")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = set_attribute {
    let key = alloc_key(scope, "setAttribute")?;
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

  scope.define_property(cache, wrapper_key, data_desc(Value::Object(wrapper)))?;

  Ok(Value::Object(wrapper))
}

fn document_document_element_get_native(
  _vm: &mut Vm,
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

  get_or_create_node_wrapper(scope, document_obj, node_id)
}

fn document_head_get_native(
  _vm: &mut Vm,
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

  get_or_create_node_wrapper(scope, document_obj, node_id)
}

fn document_body_get_native(
  _vm: &mut Vm,
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

  get_or_create_node_wrapper(scope, document_obj, node_id)
}

fn document_get_element_by_id_native(
  _vm: &mut Vm,
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
  get_or_create_node_wrapper(scope, document_obj, node_id)
}

fn document_query_selector_native(
  _vm: &mut Vm,
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
    Ok(Some(node_id)) => get_or_create_node_wrapper(scope, document_obj, node_id),
    Ok(None) => Ok(Value::Null),
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => ("NotSupportedError", message),
        crate::web::dom::DomException::InvalidStateError { message } => ("InvalidStateError", message),
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
        crate::web::dom::DomException::NotSupportedError { message } => ("NotSupportedError", message),
        crate::web::dom::DomException::InvalidStateError { message } => ("InvalidStateError", message),
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
    let wrapper = get_or_create_node_wrapper(scope, document_obj, node_id)?;
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
    Ok(Some(found)) => get_or_create_node_wrapper(scope, document_obj, found),
    Ok(None) => Ok(Value::Null),
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => ("NotSupportedError", message),
        crate::web::dom::DomException::InvalidStateError { message } => ("InvalidStateError", message),
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
        crate::web::dom::DomException::NotSupportedError { message } => ("NotSupportedError", message),
        crate::web::dom::DomException::InvalidStateError { message } => ("InvalidStateError", message),
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
    let wrapper = get_or_create_node_wrapper(scope, document_obj, node_id)?;
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
        crate::web::dom::DomException::NotSupportedError { message } => ("NotSupportedError", message),
        crate::web::dom::DomException::InvalidStateError { message } => ("InvalidStateError", message),
      };
      Err(VmError::Throw(make_dom_exception(scope, name, &message)?))
    }
  }
}

fn element_closest_native(
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
    Ok(Some(found)) => get_or_create_node_wrapper(scope, document_obj, found),
    Ok(None) => Ok(Value::Null),
    Err(err) => {
      let (name, message) = match err {
        crate::web::dom::DomException::SyntaxError { message } => ("SyntaxError", message),
        crate::web::dom::DomException::NoModificationAllowedError { message } => {
          ("NoModificationAllowedError", message)
        }
        crate::web::dom::DomException::NotSupportedError { message } => ("NotSupportedError", message),
        crate::web::dom::DomException::InvalidStateError { message } => ("InvalidStateError", message),
      };
      Err(VmError::Throw(make_dom_exception(scope, name, &message)?))
    }
  }
}

fn document_create_element_native(
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

  get_or_create_node_wrapper(scope, document_obj, node_id)
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
      if let Some(value) = scope.heap().object_get_own_data_property_value(init_obj, &bubbles_key)? {
        bubbles = scope.heap().to_boolean(value)?;
      }

      let cancelable_key = alloc_key(scope, "cancelable")?;
      if let Some(value) =
        scope
          .heap()
          .object_get_own_data_property_value(init_obj, &cancelable_key)?
      {
        cancelable = scope.heap().to_boolean(value)?;
      }

      let composed_key = alloc_key(scope, "composed")?;
      if let Some(value) = scope.heap().object_get_own_data_property_value(init_obj, &composed_key)? {
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
      if let Some(value) = scope.heap().object_get_own_data_property_value(init_obj, &bubbles_key)? {
        bubbles = scope.heap().to_boolean(value)?;
      }

      let cancelable_key = alloc_key(scope, "cancelable")?;
      if let Some(value) =
        scope
          .heap()
          .object_get_own_data_property_value(init_obj, &cancelable_key)?
      {
        cancelable = scope.heap().to_boolean(value)?;
      }

      let composed_key = alloc_key(scope, "composed")?;
      if let Some(value) = scope.heap().object_get_own_data_property_value(init_obj, &composed_key)? {
        composed = scope.heap().to_boolean(value)?;
      }

      let detail_key = alloc_key(scope, "detail")?;
      if let Some(value) = scope.heap().object_get_own_data_property_value(init_obj, &detail_key)? {
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
  scope.define_property(event_obj, cancelable_key, data_desc(Value::Bool(cancelable)))?;

  // `initEvent` does not expose `composed`; reset to false per DOM.
  let composed_key = alloc_key(scope, "composed")?;
  scope.define_property(event_obj, composed_key, data_desc(Value::Bool(false)))?;

  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  scope.define_property(event_obj, default_prevented_key, data_desc(Value::Bool(false)))?;

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
  scope.define_property(event_obj, cancelable_key, data_desc(Value::Bool(cancelable)))?;

  // `initCustomEvent` does not expose `composed`; reset to false per DOM.
  let composed_key = alloc_key(scope, "composed")?;
  scope.define_property(event_obj, composed_key, data_desc(Value::Bool(false)))?;

  let default_prevented_key = alloc_key(scope, "defaultPrevented")?;
  scope.define_property(event_obj, default_prevented_key, data_desc(Value::Bool(false)))?;

  let detail_key = alloc_key(scope, "detail")?;
  scope.define_property(event_obj, detail_key, data_desc(detail))?;

  Ok(Value::Undefined)
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

  if matches!(kind, Kind::CustomEvent) {
    let detail_key = alloc_key(scope, "detail")?;
    scope.define_property(obj, detail_key, data_desc(Value::Null))?;
  }

  Ok(Value::Object(obj))
}

fn node_append_child_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
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
  let dom = unsafe { dom_ptr.as_mut() };

  let parent_node_id = dom
    .node_id_from_index(parent_index)
    .map_err(|_| VmError::TypeError("Node.appendChild must be called on a node object"))?;
  let child_node_id = dom
    .node_id_from_index(child_index)
    .map_err(|_| VmError::TypeError("Node.appendChild requires a node argument"))?;

  if let Err(err) = dom.append_child(parent_node_id, child_node_id) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(child_value)
}

fn node_insert_before_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
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
  let dom = unsafe { dom_ptr.as_mut() };

  let parent_node_id = dom
    .node_id_from_index(parent_index)
    .map_err(|_| VmError::TypeError("Node.insertBefore must be called on a node object"))?;
  let new_child_node_id = dom
    .node_id_from_index(new_child_index)
    .map_err(|_| VmError::TypeError("Node.insertBefore requires a node argument"))?;
  let reference_node_id = match reference_index {
    Some(reference_index) => Some(dom.node_id_from_index(reference_index).map_err(|_| {
      VmError::TypeError("Node.insertBefore requires a reference node argument")
    })?),
    None => None,
  };

  if let Err(err) = dom.insert_before(parent_node_id, new_child_node_id, reference_node_id) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(new_child_value)
}

fn node_remove_child_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
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

  Ok(child_value)
}

fn node_replace_child_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
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

  if let Err(err) = dom.replace_child(parent_node_id, new_child_node_id, old_child_node_id) {
    return Err(VmError::Throw(make_dom_exception(scope, err.code(), "")?));
  }

  Ok(old_child_value)
}

fn node_clone_node_native(
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

  get_or_create_node_wrapper(scope, document_obj, cloned)
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
  _vm: &mut Vm,
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
  get_or_create_node_wrapper(scope, document_obj, node_id)
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

fn init_window_globals(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
  config: &WindowRealmConfig,
) -> Result<(Option<u64>, Option<u64>, Option<u64>), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();

  let global_this_key = alloc_key(&mut scope, "globalThis")?;
  let window_key = alloc_key(&mut scope, "window")?;
  let self_key = alloc_key(&mut scope, "self")?;
  let console_key = alloc_key(&mut scope, "console")?;
  let location_key = alloc_key(&mut scope, "location")?;
  let document_key = alloc_key(&mut scope, "document")?;

  let href_key = alloc_key(&mut scope, "href")?;
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

  let location_set_call_id = vm.register_native_call(location_set_unimplemented_native)?;
  let location_set_name = scope.alloc_string("set location")?;
  scope.push_root(Value::String(location_set_name))?;
  let location_set_func =
    scope.alloc_native_function(location_set_call_id, None, location_set_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(location_set_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(location_set_func))?;

  let protocol_get_call_id = vm.register_native_call(location_protocol_get_native)?;
  let protocol_get_name = scope.alloc_string("get protocol")?;
  scope.push_root(Value::String(protocol_get_name))?;
  let protocol_get_func =
    scope.alloc_native_function(protocol_get_call_id, None, protocol_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(protocol_get_func, Some(realm.intrinsics().function_prototype()))?;
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
  scope
    .heap_mut()
    .object_set_prototype(hostname_get_func, Some(realm.intrinsics().function_prototype()))?;
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
  scope
    .heap_mut()
    .object_set_prototype(pathname_get_func, Some(realm.intrinsics().function_prototype()))?;
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
  scope
    .heap_mut()
    .object_set_prototype(search_get_func, Some(realm.intrinsics().function_prototype()))?;
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

  let document_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(document_obj))?;
  scope.define_property(document_obj, document_url_key, data_desc(url_v))?;
  let document_location_key = alloc_key(&mut scope, "location")?;
  scope.define_property(
    document_obj,
    document_location_key,
    data_desc(Value::Object(location_obj)),
  )?;

  // document.currentScript
  let current_script_key = alloc_key(&mut scope, "currentScript")?;
  let current_script_call_id = vm.register_native_call(document_current_script_get_native)?;
  let current_script_name = scope.alloc_string("get currentScript")?;
  scope.push_root(Value::String(current_script_name))?;
  let current_script_func =
    scope.alloc_native_function(current_script_call_id, None, current_script_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(current_script_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(current_script_func))?;

  // Shared wrapper cache for returning stable element wrappers.
  let wrapper_cache = scope.alloc_object()?;
  scope.push_root(Value::Object(wrapper_cache))?;
  let wrapper_cache_key = alloc_key(&mut scope, NODE_WRAPPER_CACHE_KEY)?;
  scope.define_property(
    document_obj,
    wrapper_cache_key,
    data_desc(Value::Object(wrapper_cache)),
  )?;

  if let Some(dom_source_id) = config.dom_source_id {
    let dom_source_key = alloc_key(&mut scope, DOM_SOURCE_ID_KEY)?;
    scope.define_property(
      document_obj,
      dom_source_key,
      data_desc(Value::Number(dom_source_id as f64)),
    )?;
  }

  // document.documentElement
  let document_element_key = alloc_key(&mut scope, "documentElement")?;
  let document_element_call_id = vm.register_native_call(document_document_element_get_native)?;
  let document_element_name = scope.alloc_string("get documentElement")?;
  scope.push_root(Value::String(document_element_name))?;
  let document_element_func =
    scope.alloc_native_function(document_element_call_id, None, document_element_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  let document_head_func = scope.alloc_native_function(document_head_call_id, None, document_head_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  let document_body_func = scope.alloc_native_function(document_body_call_id, None, document_body_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(
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

  // document.getElementById
  let get_element_by_id_key = alloc_key(&mut scope, "getElementById")?;
  let get_element_by_id_call_id = vm.register_native_call(document_get_element_by_id_native)?;
  let get_element_by_id_name = scope.alloc_string("getElementById")?;
  scope.push_root(Value::String(get_element_by_id_name))?;
  let get_element_by_id_func =
    scope.alloc_native_function(get_element_by_id_call_id, None, get_element_by_id_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(
      create_element_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
  scope.push_root(Value::Object(create_element_func))?;
  scope.define_property(
    document_obj,
    create_element_key,
    data_desc(Value::Object(create_element_func)),
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
  let init_event_func = scope.alloc_native_function(init_event_call_id, None, init_event_name, 3)?;
  scope
    .heap_mut()
    .object_set_prototype(init_event_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(init_event_func))?;
  let init_event_key = alloc_key(&mut scope, "initEvent")?;
  scope.define_property(event_proto, init_event_key, data_desc(Value::Object(init_event_func)))?;

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
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(event_ctor_func, Some(realm.intrinsics().function_prototype()))?;
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
  scope.define_property(global, event_ctor_key, data_desc(Value::Object(event_ctor_func)))?;

  let custom_event_ctor_call_id = vm.register_native_call(custom_event_constructor_native)?;
  let custom_event_ctor_construct_id =
    vm.register_native_construct(custom_event_constructor_construct_native)?;
  let custom_event_ctor_name = scope.alloc_string("CustomEvent")?;
  scope.push_root(Value::String(custom_event_ctor_name))?;
  let custom_event_ctor_func =
    scope.alloc_native_function(
      custom_event_ctor_call_id,
      Some(custom_event_ctor_construct_id),
      custom_event_ctor_name,
      1,
    )?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(
      create_event_func,
      Some(realm.intrinsics().function_prototype()),
    )?;
  scope.push_root(Value::Object(create_event_func))?;
  scope.define_property(
    document_obj,
    create_event_key,
    data_desc(Value::Object(create_event_func)),
  )?;

  // Store shared Node.appendChild function on `document` so wrappers can reuse it.
  let append_child_call_id = vm.register_native_call(node_append_child_native)?;
  let append_child_name = scope.alloc_string("appendChild")?;
  scope.push_root(Value::String(append_child_name))?;
  let append_child_func =
    scope.alloc_native_function(append_child_call_id, None, append_child_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(
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
  let remove_child_func = scope.alloc_native_function(remove_child_call_id, None, remove_child_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(
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
  let clone_node_func = scope.alloc_native_function(clone_node_call_id, None, clone_node_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(
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

  // Store shared Element selector traversal APIs on `document` so wrappers can reuse them.
  let element_query_selector_call_id = vm.register_native_call(element_query_selector_native)?;
  let element_query_selector_name = scope.alloc_string("querySelector")?;
  scope.push_root(Value::String(element_query_selector_name))?;
  let element_query_selector_func =
    scope.alloc_native_function(element_query_selector_call_id, None, element_query_selector_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(
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

  let element_query_selector_all_call_id = vm.register_native_call(element_query_selector_all_native)?;
  let element_query_selector_all_name = scope.alloc_string("querySelectorAll")?;
  scope.push_root(Value::String(element_query_selector_all_name))?;
  let element_query_selector_all_func = scope.alloc_native_function(
    element_query_selector_all_call_id,
    None,
    element_query_selector_all_name,
    1,
  )?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  let element_matches_func = scope.alloc_native_function(element_matches_call_id, None, element_matches_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(element_matches_func, Some(realm.intrinsics().function_prototype()))?;
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
  let element_closest_func = scope.alloc_native_function(element_closest_call_id, None, element_closest_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(element_closest_func, Some(realm.intrinsics().function_prototype()))?;
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
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(
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

  // Store shared Element.className getter/setter functions on `document` so wrappers can reuse them.
  let class_name_get_call_id = vm.register_native_call(element_class_name_get_native)?;
  let class_name_get_name = scope.alloc_string("get className")?;
  scope.push_root(Value::String(class_name_get_name))?;
  let class_name_get_func =
    scope.alloc_native_function(class_name_get_call_id, None, class_name_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope.define_property(document_obj, id_get_key, data_desc(Value::Object(id_get_func)))?;

  let id_set_call_id = vm.register_native_call(element_id_set_native)?;
  let id_set_name = scope.alloc_string("set id")?;
  scope.push_root(Value::String(id_set_name))?;
  let id_set_func = scope.alloc_native_function(id_set_call_id, None, id_set_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(id_set_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(id_set_func))?;
  let id_set_key = alloc_key(&mut scope, ELEMENT_ID_SET_KEY)?;
  scope.define_property(document_obj, id_set_key, data_desc(Value::Object(id_set_func)))?;

  // Store shared Element.innerHTML/outerHTML accessors on `document` so wrappers can reuse them.
  let inner_html_get_call_id = vm.register_native_call(element_inner_html_get_native)?;
  let inner_html_get_name = scope.alloc_string("get innerHTML")?;
  scope.push_root(Value::String(inner_html_get_name))?;
  let inner_html_get_func =
    scope.alloc_native_function(inner_html_get_call_id, None, inner_html_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(inner_html_get_func, Some(realm.intrinsics().function_prototype()))?;
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
  scope
    .heap_mut()
    .object_set_prototype(inner_html_set_func, Some(realm.intrinsics().function_prototype()))?;
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
  scope
    .heap_mut()
    .object_set_prototype(outer_html_get_func, Some(realm.intrinsics().function_prototype()))?;
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
  scope
    .heap_mut()
    .object_set_prototype(outer_html_set_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(outer_html_set_func))?;
  let outer_html_set_key = alloc_key(&mut scope, ELEMENT_OUTER_HTML_SET_KEY)?;
  scope.define_property(
    document_obj,
    outer_html_set_key,
    data_desc(Value::Object(outer_html_set_func)),
  )?;

  // Store shared insertAdjacent* functions.
  let insert_adjacent_html_call_id = vm.register_native_call(element_insert_adjacent_html_native)?;
  let insert_adjacent_html_name = scope.alloc_string("insertAdjacentHTML")?;
  scope.push_root(Value::String(insert_adjacent_html_name))?;
  let insert_adjacent_html_func =
    scope.alloc_native_function(insert_adjacent_html_call_id, None, insert_adjacent_html_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  scope
    .heap_mut()
    .object_set_prototype(
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

  let insert_adjacent_text_call_id = vm.register_native_call(element_insert_adjacent_text_native)?;
  let insert_adjacent_text_name = scope.alloc_string("insertAdjacentText")?;
  scope.push_root(Value::String(insert_adjacent_text_name))?;
  let insert_adjacent_text_func =
    scope.alloc_native_function(insert_adjacent_text_call_id, None, insert_adjacent_text_name, 2)?;
  scope
    .heap_mut()
    .object_set_prototype(
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
  let cookie_get_func = scope.alloc_native_function(cookie_get_call_id, None, cookie_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(cookie_get_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(cookie_get_func))?;

  let cookie_set_call_id = vm.register_native_call(document_cookie_set_native)?;
  let cookie_set_name = scope.alloc_string("set cookie")?;
  scope.push_root(Value::String(cookie_set_name))?;
  let cookie_set_func = scope.alloc_native_function(cookie_set_call_id, None, cookie_set_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(cookie_set_func, Some(realm.intrinsics().function_prototype()))?;
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
  let log_call_id = vm.register_native_call(console_log_native)?;
  let log_name = scope.alloc_string("log")?;
  scope.push_root(Value::String(log_name))?;
  let log_func = scope.alloc_native_function(log_call_id, None, log_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(log_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(log_func))?;

  let log_key = alloc_key(&mut scope, "log")?;
  scope.define_property(console_obj, log_key, data_desc(Value::Object(log_func)))?;

  let error_key = alloc_key(&mut scope, "error")?;
  scope.define_property(console_obj, error_key, data_desc(Value::Object(log_func)))?;

  let console_sink_guard = config.console_sink.clone().map(ConsoleSinkGuard::new);
  if let Some(guard) = console_sink_guard.as_ref() {
    let sink_key = alloc_key(&mut scope, "__fastrender_console_sink_id")?;
    scope.define_property(
      console_obj,
      sink_key,
      data_desc(Value::Number(guard.id() as f64)),
    )?;
  }

  scope.define_property(global, global_this_key, data_desc(Value::Object(global)))?;
  scope.define_property(global, window_key, data_desc(Value::Object(global)))?;
  scope.define_property(global, self_key, data_desc(Value::Object(global)))?;

  scope.define_property(global, location_key, data_desc(Value::Object(location_obj)))?;
  scope.define_property(global, document_key, data_desc(Value::Object(document_obj)))?;
  scope.define_property(global, console_key, data_desc(Value::Object(console_obj)))?;

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
  let report_error_func =
    scope.alloc_native_function(report_error_call_id, None, report_error_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(report_error_func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(report_error_func))?;
  let report_error_key = alloc_key(&mut scope, "reportError")?;
  scope.define_property(global, report_error_key, data_desc(Value::Object(report_error_func)))?;

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

  // Install WHATWG URL bindings (`URL`/`URLSearchParams`) so real-world scripts can parse and
  // manipulate URLs. This must happen after `scope` is dropped because it borrows `heap` mutably.
  drop(scope);
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
  use std::ptr::NonNull;
  use std::sync::{Mutex as StdMutex, OnceLock as StdOnceLock};

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

  fn get_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Value {
    let key_s = scope.alloc_string(name).unwrap();
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .unwrap()
      .unwrap()
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

  #[test]
  fn window_env_shims_exist_and_match_media_evaluates() -> Result<(), VmError> {
    let media = MediaContext::screen(800.0, 600.0).with_device_pixel_ratio(2.0);
    let mut realm = WindowRealm::new(
      WindowRealmConfig::new("https://example.com/").with_media_context(media),
    )?;

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
  fn document_element_class_name_mutates_dom2_document() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html("<!doctype html><html></html>").unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/").with_dom_source_id(dom_source_id))?;
    realm.exec_script("document.documentElement.className = 'hello'")?;

    let doc_el = dom.document_element().expect("document element should exist");
    assert_eq!(dom.element_class_name(doc_el), "hello");
    Ok(())
  }

  #[test]
  fn element_inner_html_round_trips_via_window_realm_shim() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=target></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
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
  fn element_outer_html_setter_replaces_node_in_dom2_via_window_realm_shim() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=child>hi</span></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
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
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=target></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
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
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=target></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
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

    let mut realm = WindowRealm::new(
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
  fn node_remove_child_detaches_and_returns_child() -> Result<(), VmError> {
    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><html><body><div id=root><span id=child>hi</span><b id=other></b></div></body></html>",
    )
    .unwrap();
    let mut dom = Box::new(dom2::Document::from_renderer_dom(&renderer_dom));
    let dom_source_id = register_dom_source(NonNull::from(dom.as_mut()));
    let _guard = DomSourceGuard { id: dom_source_id };

    let mut realm = WindowRealm::new(
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

    let mut realm = WindowRealm::new(
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

    let mut realm = WindowRealm::new(
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

    let mut realm = WindowRealm::new(
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
  fn window_realm_shims_exist_and_are_linked() -> Result<(), VmError> {
    let url = "https://example.com/path";
    let mut realm = WindowRealm::new(WindowRealmConfig::new(url))?;

    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();

    let window = get_prop(&mut scope, global, "window");
    let global_this = get_prop(&mut scope, global, "globalThis");
    let self_ = get_prop(&mut scope, global, "self");

    assert_eq!(window, global_this);
    assert_eq!(self_, window);
    assert_eq!(window, Value::Object(global));

    let location = get_prop(&mut scope, global, "location");
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

    let document = get_prop(&mut scope, global, "document");
    let Value::Object(document_obj) = document else {
      panic!("expected object");
    };
    let doc_url = get_prop(&mut scope, document_obj, "URL");
    assert_eq!(get_string(scope.heap(), doc_url), url);

    let doc_location = get_prop(&mut scope, document_obj, "location");
    assert_eq!(doc_location, Value::Object(location_obj));

    let console = get_prop(&mut scope, global, "console");
    let Value::Object(console_obj) = console else {
      panic!("expected object");
    };
    let log = get_prop(&mut scope, console_obj, "log");
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
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/path"))?;
    let window_origin = realm.exec_script("window.origin")?;
    assert_eq!(get_string(realm.heap(), window_origin), "https://example.com");
    let origin = realm.exec_script("origin")?;
    assert_eq!(get_string(realm.heap(), origin), "https://example.com");
    assert_eq!(realm.exec_script("isSecureContext")?, Value::Bool(true));
    assert_eq!(realm.exec_script("crossOriginIsolated")?, Value::Bool(false));

    let btoa_a = realm.exec_script("btoa('a')")?;
    assert_eq!(get_string(realm.heap(), btoa_a), "YQ==");
    let atob_a = realm.exec_script("atob('YQ==')")?;
    assert_eq!(get_string(realm.heap(), atob_a), "a");
    let atob_ws = realm.exec_script("atob(' Y Q = =\\n')")?;
    assert_eq!(get_string(realm.heap(), atob_ws), "a");
    let atob_no_pad = realm.exec_script("atob('YQ')")?;
    assert_eq!(get_string(realm.heap(), atob_no_pad), "a");

    let invalid_atob = realm.exec_script("try { atob('!!!'); 'no' } catch (e) { e.name }")?;
    assert_eq!(get_string(realm.heap(), invalid_atob), "InvalidCharacterError");
    let invalid_btoa = realm.exec_script("try { btoa('\\u0100'); 'no' } catch (e) { e.name }")?;
    assert_eq!(get_string(realm.heap(), invalid_btoa), "InvalidCharacterError");

    // `reportError` must never throw (even for Symbols).
    let report_ok = realm.exec_script("try { reportError(Symbol('x')); true } catch (e) { false }")?;
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
    let sink: ConsoleSink = Arc::new(|_heap, _args| {});

    let probe = |max_bytes: usize| -> (bool, bool) {
      let before_next = NEXT_CONSOLE_SINK_ID.load(Ordering::Relaxed);

      let mut config = WindowRealmConfig::new("https://example.com/")
        .with_heap_limits(HeapLimits::new(max_bytes, max_bytes));
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
    let captured: Arc<Mutex<Vec<Vec<CapturedConsoleArg>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_sink = captured.clone();

    let sink: ConsoleSink = Arc::new(move |heap, args| {
      let entry: Vec<CapturedConsoleArg> = args
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
      captured_for_sink.lock().push(entry);
    });

    let mut config = WindowRealmConfig::new(url);
    config.console_sink = Some(sink);
    let mut realm = WindowRealm::new(config)?;

    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();

    let console = get_prop(&mut scope, global, "console");
    let Value::Object(console_obj) = console else {
      panic!("expected object");
    };
    let log = get_prop(&mut scope, console_obj, "log");

    let mut host_hooks = NoopHostHooks::default();
    let call_result = vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      log,
      Value::Object(console_obj),
      &[Value::Number(1.0), Value::Null],
    )?;
    assert_eq!(call_result, Value::Undefined);

    assert_eq!(
      &*captured.lock(),
      &[vec![
        CapturedConsoleArg::Number(1.0),
        CapturedConsoleArg::Null
      ]]
    );
    Ok(())
  }

  #[test]
  fn atob_btoa_roundtrip() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;
    let encoded = realm.exec_script("btoa('hello')")?;
    assert_eq!(get_string(realm.heap(), encoded), "aGVsbG8=");

    let decoded = realm.exec_script("atob('aGVsbG8=')")?;
    assert_eq!(get_string(realm.heap(), decoded), "hello");
    Ok(())
  }

  #[test]
  fn atob_btoa_invalid_character_errors() -> Result<(), VmError> {
    let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

    {
      let err = realm
        .exec_script("btoa('☃')")
        .expect_err("btoa should throw InvalidCharacterError for non-Latin1 input");
      let obj = unwrap_thrown_object(err);
      let (_vm, heap) = realm.vm_and_heap_mut();
      let mut scope = heap.scope();
      scope.push_root(Value::Object(obj))?;
      let name = get_prop(&mut scope, obj, "name");
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
      let name = get_prop(&mut scope, obj, "name");
      assert_eq!(get_string(scope.heap(), name), "InvalidCharacterError");
    }

    Ok(())
  }
}
