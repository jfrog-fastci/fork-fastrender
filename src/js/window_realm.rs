use crate::dom2::NodeId;
use crate::js::CurrentScriptStateHandle;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime as VmJsRuntime, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, Scope, SourceText, Value, Vm, VmError, VmHostHooks, VmOptions,
};

pub type ConsoleSink = Arc<dyn Fn(&vm_js::Heap, &[vm_js::Value]) + Send + Sync + 'static>;

#[derive(Clone)]
pub struct WindowRealmConfig {
  pub document_url: String,
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
      current_script_state: None,
      console_sink: None,
      heap_limits: default_heap_limits(),
    }
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
  console_sink_id: Option<u64>,
  current_script_source_id: Option<u64>,
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

  fn vm_js_heap_mut(&mut self) -> &mut vm_js::Heap {
    self.heap_mut()
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
    let (vm, heap) = self.vm_and_heap_mut();
    let mut scope = heap.scope();
    vm.call_with_host(&mut scope, host, callee, this, args)
  }

  fn construct(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let (vm, heap) = self.vm_and_heap_mut();
    let mut scope = heap.scope();
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

static NEXT_CURRENT_SCRIPT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);

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
  _host: &mut dyn VmHostHooks,
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

fn location_href_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
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
  _host: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "Navigation via location.href is not implemented yet",
  ))
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
  std::str::from_utf8(&buf[i..]).expect("decimal digits should be valid UTF-8")
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

  let wrapper = scope.alloc_object()?;
  scope.push_root(Value::Object(wrapper))?;

  let node_id_key = alloc_key(scope, NODE_ID_KEY)?;
  scope.define_property(
    wrapper,
    node_id_key,
    data_desc(Value::Number(node_id.index() as f64)),
  )?;

  scope.define_property(cache, wrapper_key, data_desc(Value::Object(wrapper)))?;

  Ok(Value::Object(wrapper))
}

fn document_current_script_get_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
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
  let document_url_key = alloc_key(&mut scope, "URL")?;

  let url_s = scope.alloc_string(&config.document_url)?;
  scope.push_root(Value::String(url_s))?;
  let url_v = Value::String(url_s);

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

  Ok((
    console_sink_guard.map(ConsoleSinkGuard::disarm),
    current_script_guard.map(CurrentScriptSourceGuard::disarm),
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Mutex as StdMutex, OnceLock as StdOnceLock};

  #[derive(Debug, Clone, PartialEq)]
  enum CapturedConsoleArg {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    BigInt,
    String(String),
    BigInt,
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

  fn console_sink_test_lock() -> &'static StdMutex<()> {
    static LOCK: StdOnceLock<StdMutex<()>> = StdOnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
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
          Value::BigInt(_) => CapturedConsoleArg::BigInt,
          Value::String(s) => CapturedConsoleArg::String(
            heap
              .get_string(s)
              .expect("string handle should be valid")
              .to_utf8_lossy(),
          ),
          Value::BigInt(_) => CapturedConsoleArg::BigInt,
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
}
