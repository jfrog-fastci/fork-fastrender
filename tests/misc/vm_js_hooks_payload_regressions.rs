use fastrender::error::Error;
use fastrender::js::window_timers::VmJsEventLoopHooks;
use fastrender::js::{
  install_window_timers_bindings, EventLoop, RunLimits, VirtualClock, VmJsModuleLoader,
  WindowRealm, WindowRealmConfig, WindowRealmHost,
};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::Result as FrResult;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use vm_js::{
  GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks,
};
use webidl_vm_js::{host_from_hooks, WebIdlBindingsHost};

#[derive(Debug, Default)]
struct CounterWebIdlHost {
  calls: usize,
}

impl WebIdlBindingsHost for CounterWebIdlHost {
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
    self.calls = self.calls.saturating_add(1);
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
    self.calls = self.calls.saturating_add(1);
    Ok(Value::Undefined)
  }
}

#[derive(Debug, Default)]
struct VmHostCtx;

// `VmHost` is implemented for all `T: Any`, so no explicit impl is required.

struct HooksRegressionHost {
  vm_host: VmHostCtx,
  webidl: CounterWebIdlHost,
  window: WindowRealm,
}

impl HooksRegressionHost {
  fn new(clock: Arc<VirtualClock>) -> FrResult<Self> {
    let mut window =
      WindowRealm::new(WindowRealmConfig::new("https://example.com/").with_clock(clock))
        .map_err(|err| Error::Other(err.to_string()))?;
    {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<Self>(vm, realm, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      install_dispatch_global(vm, realm, heap).map_err(|err| Error::Other(err.to_string()))?;
      install_assert_host_ctx_global(vm, realm, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
    }
    Ok(Self {
      vm_host: VmHostCtx,
      webidl: CounterWebIdlHost::default(),
      window,
    })
  }
}

impl WindowRealmHost for HooksRegressionHost {
  fn vm_host_and_window_realm(&mut self) -> FrResult<(&mut dyn VmHost, &mut WindowRealm)> {
    Ok((&mut self.vm_host, &mut self.window))
  }

  fn webidl_bindings_host(&mut self) -> Option<&mut dyn WebIdlBindingsHost> {
    Some(&mut self.webidl)
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

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn install_dispatch_global(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut vm_js::Heap,
) -> Result<(), VmError> {
  let call_id = vm.register_native_call(dispatch_via_webidl_host_from_hooks)?;
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let name_key = alloc_key(&mut scope, "__dispatch")?;
  let PropertyKey::String(name_str) = name_key else {
    return Err(VmError::InvariantViolation(
      "expected __dispatch key to be a string",
    ));
  };
  let func = scope.alloc_native_function(call_id, None, name_str, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(func))?;

  scope.define_property(global, name_key, data_desc(Value::Object(func)))?;
  Ok(())
}

fn install_assert_host_ctx_global(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut vm_js::Heap,
) -> Result<(), VmError> {
  fn assert_host_ctx_native(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    if host.as_any_mut().downcast_mut::<VmHostCtx>().is_none() {
      return Err(VmError::TypeError("expected VmHostCtx"));
    }
    Ok(Value::Undefined)
  }

  let call_id = vm.register_native_call(assert_host_ctx_native)?;
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let name_key = alloc_key(&mut scope, "__assert_host_ctx")?;
  let PropertyKey::String(name_str) = name_key else {
    return Err(VmError::InvariantViolation(
      "expected __assert_host_ctx key to be a string",
    ));
  };
  let func = scope.alloc_native_function(call_id, None, name_str, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(func))?;

  scope.define_property(global, name_key, data_desc(Value::Object(func)))?;
  Ok(())
}

fn dispatch_via_webidl_host_from_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let host = host_from_hooks(hooks)?;
  host.call_operation(vm, scope, None, "HooksRegression", "dispatch", 0, &[])?;
  Ok(Value::Undefined)
}

#[test]
fn webidl_dispatch_works_in_top_level_script() -> FrResult<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut host = HooksRegressionHost::new(clock.clone())?;
  let mut event_loop = EventLoop::<HooksRegressionHost>::with_clock(clock);

  let mut hooks = VmJsEventLoopHooks::<HooksRegressionHost>::new_with_host(&mut host)?;
  hooks.set_event_loop(&mut event_loop);
  let (host_ctx, window) = host.vm_host_and_window_realm()?;
  window.reset_interrupt();
  window
    .exec_script_with_host_and_hooks(host_ctx, &mut hooks, "__dispatch();")
    .map_err(|err| Error::Other(err.to_string()))?;
  if let Some(err) = hooks.finish(window.heap_mut()) {
    return Err(err);
  }

  assert_eq!(host.webidl.calls, 1);
  Ok(())
}

#[test]
fn webidl_dispatch_works_in_promise_jobs() -> FrResult<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut host = HooksRegressionHost::new(clock.clone())?;
  let mut event_loop = EventLoop::<HooksRegressionHost>::with_clock(clock);

  let mut hooks = VmJsEventLoopHooks::<HooksRegressionHost>::new_with_host(&mut host)?;
  hooks.set_event_loop(&mut event_loop);
  let (host_ctx, window) = host.vm_host_and_window_realm()?;
  window.reset_interrupt();
  window
    .exec_script_with_host_and_hooks(
      host_ctx,
      &mut hooks,
      "__dispatch(); Promise.resolve().then(() => { __dispatch(); });",
    )
    .map_err(|err| Error::Other(err.to_string()))?;
  if let Some(err) = hooks.finish(window.heap_mut()) {
    return Err(err);
  }

  event_loop.perform_microtask_checkpoint(&mut host)?;
  assert_eq!(host.webidl.calls, 2);
  Ok(())
}

#[test]
fn webidl_dispatch_works_in_timer_callbacks() -> FrResult<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut host = HooksRegressionHost::new(clock.clone())?;
  let mut event_loop = EventLoop::<HooksRegressionHost>::with_clock(clock.clone());

  let mut hooks = VmJsEventLoopHooks::<HooksRegressionHost>::new_with_host(&mut host)?;
  hooks.set_event_loop(&mut event_loop);
  let (host_ctx, window) = host.vm_host_and_window_realm()?;
  window.reset_interrupt();
  window
    .exec_script_with_host_and_hooks(
      host_ctx,
      &mut hooks,
      "__dispatch(); setTimeout(() => { __dispatch(); }, 0);",
    )
    .map_err(|err| Error::Other(err.to_string()))?;
  if let Some(err) = hooks.finish(window.heap_mut()) {
    return Err(err);
  }

  // Ensure the timeout is due.
  clock.advance(Duration::from_millis(1));
  event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

  assert_eq!(host.webidl.calls, 2);
  Ok(())
}

#[derive(Default)]
struct MapFetcher {
  map: HashMap<String, FetchedResource>,
}

impl ResourceFetcher for MapFetcher {
  fn fetch(&self, url: &str) -> FrResult<FetchedResource> {
    self
      .map
      .get(url)
      .cloned()
      .ok_or_else(|| Error::Other(format!("no fixture for url {url}")))
  }
}

#[test]
fn webidl_dispatch_works_during_module_evaluation() -> FrResult<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut host = HooksRegressionHost::new(clock.clone())?;
  let mut event_loop = EventLoop::<HooksRegressionHost>::with_clock(clock);

  let entry_url = "https://example.com/entry.js";
  let dep_url = "https://example.com/dep.js";

  let mut fetcher = MapFetcher::default();
  fetcher.map.insert(
    entry_url.to_string(),
    FetchedResource::new(
      format!(
        "import {{ value }} from './dep.js';\n\
         globalThis.result = value;\n\
         __dispatch();\n"
      )
      .into_bytes(),
      Some("application/javascript".to_string()),
    ),
  );
  fetcher.map.insert(
    dep_url.to_string(),
    FetchedResource::new(
      "export const value = 1;\n__dispatch();\n"
        .as_bytes()
        .to_vec(),
      Some("application/javascript".to_string()),
    ),
  );

  let mut loader = VmJsModuleLoader::new(Arc::new(fetcher), "https://example.com/");
  let _ = loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;

  assert_eq!(host.webidl.calls, 2);
  Ok(())
}

#[test]
fn vm_host_is_available_during_module_evaluation() -> FrResult<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut host = HooksRegressionHost::new(clock.clone())?;
  let mut event_loop = EventLoop::<HooksRegressionHost>::with_clock(clock);

  let entry_url = "https://example.com/entry.js";
  let dep_url = "https://example.com/dep.js";

  let mut fetcher = MapFetcher::default();
  fetcher.map.insert(
    entry_url.to_string(),
    FetchedResource::new(
      format!(
        "import {{ value }} from './dep.js';\n\
         __assert_host_ctx();\n\
         globalThis.result = value;\n"
      )
      .into_bytes(),
      Some("application/javascript".to_string()),
    ),
  );
  fetcher.map.insert(
    dep_url.to_string(),
    FetchedResource::new(
      "__assert_host_ctx();\nexport const value = 1;\n"
        .as_bytes()
        .to_vec(),
      Some("application/javascript".to_string()),
    ),
  );

  let mut loader = VmJsModuleLoader::new(Arc::new(fetcher), "https://example.com/");
  let _ = loader.evaluate_module_url(&mut host, &mut event_loop, entry_url)?;

  Ok(())
}
