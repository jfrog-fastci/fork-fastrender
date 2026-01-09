use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use vm_js::{
  Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmHostHooks, VmOptions,
};

pub type ConsoleSink =
  Arc<dyn Fn(&vm_js::Heap, &[vm_js::Value]) + Send + Sync + 'static>;

#[derive(Clone)]
pub struct WindowRealmConfig {
  pub document_url: String,
  pub console_sink: Option<ConsoleSink>,
}

impl WindowRealmConfig {
  pub fn new(document_url: impl Into<String>) -> Self {
    Self {
      document_url: document_url.into(),
      console_sink: None,
    }
  }
}

pub struct WindowRealm {
  vm: Vm,
  heap: Heap,
  realm: Realm,
  console_sink_id: Option<u64>,
}

impl WindowRealm {
  pub fn new(config: WindowRealmConfig) -> Result<Self, VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(default_heap_limits());
    let realm = Realm::new(&mut heap)?;
    let console_sink_id = init_window_globals(&mut vm, &mut heap, &realm, &config)?;
    Ok(Self {
      vm,
      heap,
      realm,
      console_sink_id,
    })
  }

  pub fn heap(&self) -> &Heap {
    &self.heap
  }

  pub fn heap_mut(&mut self) -> &mut Heap {
    &mut self.heap
  }

  pub fn vm(&self) -> &Vm {
    &self.vm
  }

  pub fn vm_mut(&mut self) -> &mut Vm {
    &mut self.vm
  }

  pub fn vm_and_heap_mut(&mut self) -> (&mut Vm, &mut Heap) {
    (&mut self.vm, &mut self.heap)
  }

  pub fn realm(&self) -> &Realm {
    &self.realm
  }

  pub fn realm_mut(&mut self) -> &mut Realm {
    &mut self.realm
  }

  pub fn global_object(&self) -> vm_js::GcObject {
    self.realm.global_object()
  }

  pub fn teardown(&mut self) {
    if let Some(id) = self.console_sink_id.take() {
      unregister_console_sink(id);
    }
    self.realm.teardown(&mut self.heap);
  }
}

impl Drop for WindowRealm {
  fn drop(&mut self) {
    self.teardown();
  }
}

fn default_heap_limits() -> HeapLimits {
  HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024)
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
  scope.push_root(Value::String(s));
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

fn console_log_native(
  _vm: &mut Vm,
  _host: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
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

fn init_window_globals(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
  config: &WindowRealmConfig,
) -> Result<Option<u64>, VmError> {
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
  scope.push_root(Value::String(url_s));
  let url_v = Value::String(url_s);

  let location_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(location_obj));
  scope.define_property(location_obj, href_key, data_desc(url_v))?;

  let document_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(document_obj));
  scope.define_property(document_obj, document_url_key, data_desc(url_v))?;
  let document_location_key = alloc_key(&mut scope, "location")?;
  scope.define_property(
    document_obj,
    document_location_key,
    data_desc(Value::Object(location_obj)),
  )?;

  let console_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(console_obj));

  let log_call_id = vm.register_native_call(console_log_native);
  let log_name = scope.alloc_string("log")?;
  scope.push_root(Value::String(log_name));
  let log_func = scope.alloc_native_function(log_call_id, None, log_name, 0)?;
  scope.push_root(Value::Object(log_func));

  let log_key = alloc_key(&mut scope, "log")?;
  scope.define_property(console_obj, log_key, data_desc(Value::Object(log_func)))?;

  let error_key = alloc_key(&mut scope, "error")?;
  scope.define_property(console_obj, error_key, data_desc(Value::Object(log_func)))?;

  let console_sink_id = config.console_sink.clone().map(register_console_sink);
  if let Some(id) = console_sink_id {
    let sink_key = alloc_key(&mut scope, "__fastrender_console_sink_id")?;
    scope.define_property(console_obj, sink_key, data_desc(Value::Number(id as f64)))?;
  }

  scope.define_property(global, global_this_key, data_desc(Value::Object(global)))?;
  scope.define_property(global, window_key, data_desc(Value::Object(global)))?;
  scope.define_property(global, self_key, data_desc(Value::Object(global)))?;

  scope.define_property(
    global,
    location_key,
    data_desc(Value::Object(location_obj)),
  )?;
  scope.define_property(
    global,
    document_key,
    data_desc(Value::Object(document_obj)),
  )?;
  scope.define_property(
    global,
    console_key,
    data_desc(Value::Object(console_obj)),
  )?;

  Ok(console_sink_id)
}

#[cfg(test)]
mod tests {
  use super::*;
  use vm_js::VmHostHooks;

  #[derive(Default)]
  struct NoopHostHooks;

  impl VmHostHooks for NoopHostHooks {
    fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {
      // This test only calls synchronous native functions.
    }
  }

  fn get_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  fn get_prop(scope: &mut Scope<'_>, obj: vm_js::GcObject, name: &str) -> Value {
    let key_s = scope.alloc_string(name).unwrap();
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .unwrap()
      .unwrap()
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
    let href = get_prop(&mut scope, location_obj, "href");
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
    let mut hooks = NoopHostHooks::default();
    let call_result = vm.call(
      &mut hooks,
      &mut scope,
      log,
      Value::Object(console_obj),
      &[Value::Number(1.0), Value::Null],
    )?;
    assert_eq!(call_result, Value::Undefined);

    Ok(())
  }
}
