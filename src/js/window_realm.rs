use crate::dom2::{self, NodeId};
use crate::js::CurrentScriptStateHandle;
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
      dom_source_id: None,
      current_script_state: None,
      console_sink: None,
      heap_limits: default_heap_limits(),
    }
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
}

impl WindowRealm {
  pub fn new(config: WindowRealmConfig) -> Result<Self, VmError> {
    let realm_id = RealmId::from_raw(NEXT_WINDOW_REALM_ID.fetch_add(1, Ordering::Relaxed));
    let mut vm_options = VmOptions::default();
    // Window realms should be interruptible even before full script execution is wired up.
    // This is separate from the renderer-level interrupt flag; callers can wire it up as needed.
    vm_options.interrupt_flag = Some(Arc::new(AtomicBool::new(false)));
    let vm = Vm::new(vm_options);
    let heap = Heap::new(config.heap_limits);

    let mut runtime = VmJsRuntime::new(vm, heap)?;

    // `vm-js::JsRuntime` does not expose a borrow-splitting accessor for `(vm, realm, heap)`. Use a
    // raw pointer to the realm to allow simultaneously borrowing `vm`/`heap` mutably.
    //
    // SAFETY: `vm-js::JsRuntime` stores `vm`, `heap`, and `realm` as disjoint fields. We do not
    // move the runtime while these borrows are live.
    let realm_ptr = runtime.realm() as *const Realm;
    let (vm, heap) = (&mut runtime.vm, &mut runtime.heap);
    let realm = unsafe { &*realm_ptr };

    let (console_sink_id, current_script_source_id) = init_window_globals(vm, heap, realm, &config)?;
    Ok(Self {
      runtime,
      realm_id,
      console_sink_id,
      current_script_source_id,
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
  }

  /// Execute a classic script in this window realm.
  pub fn exec_script(&mut self, source: &str) -> Result<Value, VmError> {
    self.runtime.exec_script(source)
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
static NEXT_WINDOW_REALM_ID: AtomicU64 = AtomicU64::new(1);
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
const ELEMENT_CLASS_NAME_GET_KEY: &str = "__fastrender_element_class_name_get";
const ELEMENT_CLASS_NAME_SET_KEY: &str = "__fastrender_element_class_name_set";
const ELEMENT_ID_GET_KEY: &str = "__fastrender_element_id_get";
const ELEMENT_ID_SET_KEY: &str = "__fastrender_element_id_set";
const NODE_APPEND_CHILD_KEY: &str = "__fastrender_node_append_child";
const ELEMENT_GET_ATTRIBUTE_KEY: &str = "__fastrender_element_get_attribute";
const ELEMENT_SET_ATTRIBUTE_KEY: &str = "__fastrender_element_set_attribute";

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
  let get_attribute = {
    let key = alloc_key(scope, ELEMENT_GET_ATTRIBUTE_KEY)?;
    scope.heap().object_get_own_data_property_value(document_obj, &key)?
  };
  let set_attribute = {
    let key = alloc_key(scope, ELEMENT_SET_ATTRIBUTE_KEY)?;
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

  if let Value::Number(_) = dom_source_id_value {
    scope.define_property(wrapper, dom_source_id_key, data_desc(dom_source_id_value))?;
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

  if let Some(Value::Object(func)) = get_attribute {
    let key = alloc_key(scope, "getAttribute")?;
    scope.define_property(wrapper, key, data_desc(Value::Object(func)))?;
  }

  if let Some(Value::Object(func)) = set_attribute {
    let key = alloc_key(scope, "setAttribute")?;
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

fn init_window_globals(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
  config: &WindowRealmConfig,
) -> Result<(Option<u64>, Option<u64>), VmError> {
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

  Ok((
    console_sink_guard.map(ConsoleSinkGuard::disarm),
    current_script_guard.map(CurrentScriptSourceGuard::disarm),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
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
    match err {
      VmError::Throw(Value::Object(obj)) => obj,
      VmError::Throw(other) => panic!("expected thrown object, got {other:?}"),
      other => panic!("expected VmError::Throw, got {other:?}"),
    }
  }

  fn console_sink_test_lock() -> &'static StdMutex<()> {
    static LOCK: StdOnceLock<StdMutex<()>> = StdOnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
  }

  #[test]
  fn document_element_class_name_mutates_dom2_document() -> Result<(), VmError> {
    struct DomSourceGuard {
      id: u64,
    }

    impl Drop for DomSourceGuard {
      fn drop(&mut self) {
        unregister_dom_source(self.id);
      }
    }

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
          Value::BigInt(b) => CapturedConsoleArg::BigInt(b.to_decimal_string()),
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
