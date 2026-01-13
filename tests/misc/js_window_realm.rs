use fastrender::dom2::{Document as Dom2Document, NodeId, NodeKind};
use fastrender::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
use fastrender::js::window_timers::VmJsEventLoopHooks;
use fastrender::js::{
  DocumentHostState, DomHost, EventLoop, JsExecutionOptions, QueueLimits, RunLimits,
  RunUntilIdleOutcome, ScriptBlockExecutor, ScriptOrchestrator, ScriptType, TaskSource,
  VirtualClock, WindowFetchEnv, WindowHost, WindowHostState, WindowRealm, WindowRealmConfig,
  WindowRealmHost,
};
use fastrender::js::webidl::VmJsWebIdlBindingsHostDispatch;
use fastrender::js::window_realm::DomBindingsBackend;
use fastrender::render_control;
use fastrender::resource::web_fetch::WebFetchLimits;
use fastrender::resource::{
  FetchCredentialsMode, FetchDestination, FetchRequest, FetchedResource, HttpRequest,
  ResourceFetcher,
};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vm_js::{
  Heap, Job, PropertyKey, RealmId, Scope, TerminationReason, Value, Vm, VmError, VmHost,
  VmHostHooks,
};

const ASSERT_VM_HOST_FN_NAME: &str = "__fastrender_assert_vm_host";

fn install_vm_js_microtask_checkpoint_hook<Host: WindowRealmHost>(
  event_loop: &mut EventLoop<Host>,
) {
  fn drain<Host: WindowRealmHost>(
    host: &mut Host,
    _event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    let realm = host.window_realm()?;
    realm
      .perform_microtask_checkpoint()
      .map_err(|err| Error::Other(err.to_string()))?;
    Ok(())
  }

  event_loop.set_microtask_checkpoint_hook(Some(drain::<Host>));
}

fn js_opts_for_test() -> JsExecutionOptions {
  // `vm-js` budgets are based on wall-clock time. The library default is intentionally aggressive,
  // but under parallel `cargo test` the OS can deschedule a test thread long enough for the VM to
  // observe a false-positive deadline exceed. Use a generous limit to keep integration tests
  // deterministic while still bounding infinite loops.
  let mut opts = JsExecutionOptions::default();
  opts.event_loop_run_limits.max_wall_time = Some(std::time::Duration::from_secs(5));
  opts
}

#[derive(Debug, Default)]
struct NoFetchResourceFetcher;

impl ResourceFetcher for NoFetchResourceFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    Err(Error::Other(format!(
      "NoFetchResourceFetcher does not support fetch: {url}"
    )))
  }
}

fn make_host(dom: Dom2Document, document_url: impl Into<String>) -> Result<WindowHost> {
  WindowHost::new_with_fetcher_and_options(
    dom,
    document_url,
    Arc::new(NoFetchResourceFetcher),
    js_opts_for_test(),
  )
}

fn host_state_from_renderer_dom(
  renderer_dom: &fastrender::dom::DomNode,
  document_url: impl Into<String>,
) -> Result<WindowHostState> {
  WindowHostState::new_with_fetcher_and_options(
    Dom2Document::from_renderer_dom(renderer_dom),
    document_url,
    Arc::new(InMemoryFetcher::default()),
    js_opts_for_test(),
  )
}

fn host_state_with_fetcher(
  dom: Dom2Document,
  document_url: impl Into<String>,
  fetcher: Arc<dyn ResourceFetcher>,
) -> Result<WindowHostState> {
  WindowHostState::new_with_fetcher_and_options(dom, document_url, fetcher, js_opts_for_test())
}

fn exec_script_in_window_host(
  host: &mut WindowHostState,
  source: &str,
) -> std::result::Result<Value, VmError> {
  // Use `exec_script_with_host_and_hooks` so native call handlers receive a real `VmHost` context
  // (required for fetch/XHR bindings).
  //
  // Provide a temporary `EventLoop` so Promise jobs can be queued/discarded safely even though this
  // helper doesn't drive the event loop.
  let mut event_loop = EventLoop::<WindowHostState>::new();
  let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new_with_host(host)
    .expect("exec_script_in_window_host: create VmJsEventLoopHooks");
  hooks.set_event_loop(&mut event_loop);
  let (vm_host, window) = host
    .vm_host_and_window_realm()
    .expect("exec_script_in_window_host: vm_host_and_window_realm");
  let result = window.exec_script_with_host_and_hooks(vm_host, &mut hooks, source);
  if let Some(err) = hooks.finish(window.heap_mut()) {
    panic!("exec_script_in_window_host: VmHostHooks finish returned error: {err}");
  }
  result
}

struct WebIdlWindowHostStateForTest {
  document: DocumentHostState,
  window: WindowRealm,
  webidl_bindings_host: VmJsWebIdlBindingsHostDispatch<WebIdlWindowHostStateForTest>,
}

impl WebIdlWindowHostStateForTest {
  fn from_renderer_dom(
    renderer_dom: &fastrender::dom::DomNode,
    document_url: impl Into<String>,
  ) -> Result<Self> {
    let document_url = document_url.into();
    let document = DocumentHostState::new(Dom2Document::from_renderer_dom(renderer_dom));
    let window = WindowRealm::new_with_js_execution_options(
      WindowRealmConfig::new(document_url)
        .with_dom_bindings_backend(DomBindingsBackend::WebIdl)
        .with_current_script_state(document.current_script_handle().clone()),
      js_opts_for_test(),
    )
    .map_err(|err| Error::Other(err.to_string()))?;
    let webidl_bindings_host = VmJsWebIdlBindingsHostDispatch::new(window.global_object());
    Ok(Self {
      document,
      window,
      webidl_bindings_host,
    })
  }
}

impl DomHost for WebIdlWindowHostStateForTest {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Dom2Document) -> R,
  {
    self.document.with_dom(f)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Dom2Document) -> (R, bool),
  {
    self.document.mutate_dom(f)
  }
}

impl WindowRealmHost for WebIdlWindowHostStateForTest {
  fn vm_host_and_window_realm(
    &mut self,
  ) -> Result<(&mut dyn VmHost, &mut WindowRealm)> {
    Ok((&mut self.document, &mut self.window))
  }

  fn webidl_bindings_host(
    &mut self,
  ) -> Option<&mut dyn webidl_vm_js::WebIdlBindingsHost> {
    Some(&mut self.webidl_bindings_host)
  }
}

fn exec_script_in_webidl_window_host(
  host: &mut WebIdlWindowHostStateForTest,
  source: &str,
) -> Result<Value> {
  // Provide a temporary `EventLoop` so Promise jobs can be queued/discarded safely even though this
  // helper doesn't drive the event loop.
  let mut event_loop = EventLoop::<WebIdlWindowHostStateForTest>::new();
  let mut hooks = VmJsEventLoopHooks::<WebIdlWindowHostStateForTest>::new_with_host(host)?;
  hooks.set_event_loop(&mut event_loop);

  let (vm_host, realm) = host.vm_host_and_window_realm()?;
  let result = realm.exec_script_with_host_and_hooks(vm_host, &mut hooks, source);

  if let Some(err) = hooks.finish(realm.heap_mut()) {
    return Err(err);
  }

  match result {
    Ok(value) => Ok(value),
    Err(err) => Err(Error::Other(format_vm_error(realm.heap_mut(), err))),
  }
}

fn get_string(heap: &Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn format_vm_error(heap: &mut Heap, err: VmError) -> String {
  if let Some(value) = err.thrown_value() {
    if let Value::String(s) = value {
      if let Ok(js) = heap.get_string(s) {
        return js.to_utf8_lossy();
      }
    }

    if let Value::Object(obj) = value {
      let mut scope = heap.scope();
      scope.push_root(Value::Object(obj)).ok();

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
          Value::String(s) => Some(scope.heap().get_string(s).ok()?.to_utf8_lossy()),
          _ => None,
        }
      };

      let name = get_prop_str("name");
      let message = get_prop_str("message");
      match (name, message) {
        (Some(name), Some(message)) if !message.is_empty() => format!("{name}: {message}"),
        (Some(name), _) => name,
        (_, Some(message)) => message,
        _ => "uncaught exception".to_string(),
      }
    } else {
      "uncaught exception".to_string()
    }
  } else {
    match err {
      VmError::Syntax(diags) => format!("syntax error: {diags:?}"),
      other => other.to_string(),
    }
  }
}

fn get_data_prop(scope: &mut Scope<'_>, obj: vm_js::GcObject, name: &str) -> Value {
  let key_s = scope.alloc_string(name).unwrap();
  let key = PropertyKey::from_string(key_s);
  scope
    .heap()
    .object_get_own_data_property_value(obj, &key)
    .unwrap()
    .unwrap()
}

fn install_assert_non_dummy_vm_host(host: &mut WindowHostState) -> Result<()> {
  fn assert_vm_host_native(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    _hooks: &mut dyn vm_js::VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    if std::mem::size_of_val(&*host) == 0 {
      Err(VmError::TypeError("callback invoked with dummy VmHost"))
    } else {
      Ok(Value::Undefined)
    }
  }

  let window = host.window_mut();
  let (vm, realm, heap) = window.vm_realm_and_heap_mut();
  let call_id = vm
    .register_native_call(assert_vm_host_native)
    .map_err(|e| Error::Other(e.to_string()))?;

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope
    .push_root(Value::Object(global))
    .map_err(|e| Error::Other(e.to_string()))?;

  let name_s = scope
    .alloc_string(ASSERT_VM_HOST_FN_NAME)
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::String(name_s))
    .map_err(|e| Error::Other(e.to_string()))?;

  let func = scope
    .alloc_native_function(call_id, None, name_s, 0)
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::Object(func))
    .map_err(|e| Error::Other(e.to_string()))?;

  let key = PropertyKey::from_string(name_s);
  scope
    .define_property(
      global,
      key,
      vm_js::PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: vm_js::PropertyKind::Data {
          value: Value::Object(func),
          writable: true,
        },
      },
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  Ok(())
}

#[test]
fn document_host_state_is_threaded_through_window_entry_points() -> Result<()> {
  use fastrender::js::DocumentHostState;
  use vm_js::{Scope, Value, Vm, VmError, VmHost};

  const HOST_CONTEXT_DOWNCAST_ERROR: &str = "VmHost is not DocumentHostState";

  fn host_ctx_tick_native(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    _hooks: &mut dyn vm_js::VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let Some(ctx) = host.as_any_mut().downcast_mut::<DocumentHostState>() else {
      return Err(VmError::TypeError(HOST_CONTEXT_DOWNCAST_ERROR));
    };
    let _ = ctx.dom().root();
    Ok(Value::Number(1.0))
  }

  fn install_host_ctx_tick(host: &mut WindowHost) -> Result<()> {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .map_err(|e| Error::Other(e.to_string()))?;

    let id = vm
      .register_native_call(host_ctx_tick_native)
      .map_err(|e| Error::Other(e.to_string()))?;

    let name_s = scope
      .alloc_string("__host_ctx_tick")
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .push_root(Value::String(name_s))
      .map_err(|e| Error::Other(e.to_string()))?;
    let func = scope
      .alloc_native_function(id, None, name_s, 0)
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .push_root(Value::Object(func))
      .map_err(|e| Error::Other(e.to_string()))?;

    let key = PropertyKey::from_string(name_s);
    scope
      .define_property(
        global,
        key,
        vm_js::PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: vm_js::PropertyKind::Data {
            value: Value::Object(func),
            writable: true,
          },
        },
      )
      .map_err(|e| Error::Other(e.to_string()))?;

    Ok(())
  }

  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(InMemoryFetcher::default().with_response("https://example.com/x", Vec::new(), 200));
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_fetcher_and_options(
    dom,
    "https://example.com/",
    fetcher,
    js_opts_for_test(),
  )?;
  install_host_ctx_tick(&mut host)?;

  host.exec_script(
    r#"
globalThis.__count = 0;
function ping() { globalThis.__count += __host_ctx_tick(); }
ping();
"#,
  )?;

  host.exec_script("Promise.resolve().then(ping);")?;
  host.exec_script("setTimeout(ping, 0);")?;
  host.exec_script("requestAnimationFrame(ping);")?;
  host.exec_script("fetch(\"https://example.com/x\").then(ping);")?;

  assert_eq!(
    host.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  // `run_until_idle` intentionally does not run animation frames. Queue an explicit task that runs
  // one frame turn so the callback fires.
  host.queue_task(TaskSource::Script, |host, event_loop| {
    let _ = event_loop.run_animation_frame(host)?;
    Ok(())
  })?;
  assert_eq!(
    host.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let count = host.exec_script("globalThis.__count")?;
  let Value::Number(n) = count else {
    return Err(Error::Other(format!(
      "expected globalThis.__count to be a number, got {count:?}"
    )));
  };
  assert_eq!(n, 5.0);

  Ok(())
}

fn find_script_elements(dom: &Dom2Document) -> Vec<NodeId> {
  dom
    .subtree_preorder(dom.root())
    .filter(|&id| matches!(&dom.node(id).kind, NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("script")))
    .collect()
}

fn inline_script_text(dom: &Dom2Document, script: NodeId) -> String {
  let node = dom.node(script);
  node
    .children
    .iter()
    .filter_map(|&child| match &dom.node(child).kind {
      NodeKind::Text { content } => Some(content.as_str()),
      _ => None,
    })
    .collect::<String>()
}

fn get_current_script(
  vm: &mut Vm,
  heap: &mut Heap,
  document_obj: vm_js::GcObject,
) -> Result<Value> {
  let mut scope = heap.scope();
  let key_s = scope
    .alloc_string("currentScript")
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::String(key_s))
    .map_err(|e| Error::Other(e.to_string()))?;
  let key = PropertyKey::from_string(key_s);
  vm.get(&mut scope, document_obj, key)
    .map_err(|e| Error::Other(e.to_string()))
}

fn get_wrapper_node_id(vm: &mut Vm, heap: &mut Heap, wrapper: vm_js::GcObject) -> Result<usize> {
  let mut scope = heap.scope();
  let key_s = scope
    .alloc_string("__fastrender_node_id")
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::String(key_s))
    .map_err(|e| Error::Other(e.to_string()))?;
  let key = PropertyKey::from_string(key_s);
  let value = vm
    .get(&mut scope, wrapper, key)
    .map_err(|e| Error::Other(e.to_string()))?;
  let Value::Number(n) = value else {
    return Err(Error::Other(
      "expected __fastrender_node_id to be a number".to_string(),
    ));
  };
  Ok(n as usize)
}

#[test]
fn window_self_and_document_url_are_exposed() -> Result<()> {
  let url = "https://example.com/";
  let mut realm =
    WindowRealm::new_with_js_execution_options(WindowRealmConfig::new(url), js_opts_for_test())
      .map_err(|e| Error::Other(e.to_string()))?;

  let global = realm.global_object();
  let (_vm, heap) = realm.vm_and_heap_mut();
  let mut scope = heap.scope();

  let window = get_data_prop(&mut scope, global, "window");
  let self_ = get_data_prop(&mut scope, global, "self");
  assert_eq!(window, Value::Object(global));
  assert_eq!(self_, Value::Object(global));

  let document = get_data_prop(&mut scope, global, "document");
  let Value::Object(document_obj) = document else {
    panic!("expected document to be an object");
  };

  let doc_url = get_data_prop(&mut scope, document_obj, "URL");
  assert_eq!(get_string(scope.heap(), doc_url), url);
  Ok(())
}

#[test]
fn document_base_uri_falls_back_to_document_url() -> Result<()> {
  let url = "https://example.com/";
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new(url)).map_err(|e| Error::Other(e.to_string()))?;

  // Simulate an embedder that has not yet installed a base URL (or cleared it while navigating).
  realm.set_base_url(None);

  let base_uri = realm
    .exec_script("document.baseURI")
    .map_err(|e| Error::Other(e.to_string()))?;

  let (_vm, heap) = realm.vm_and_heap_mut();
  assert_eq!(get_string(heap, base_uri), url);
  Ok(())
}

#[test]
fn document_default_view_points_at_window() -> Result<()> {
  let url = "https://example.com/";
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new(url)).map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script("document.defaultView === window && document.defaultView === self")
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn document_charset_properties_are_exposed() -> Result<()> {
  let url = "https://example.com/";
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new(url)).map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script("document.characterSet + '|' + document.charset + '|' + document.inputEncoding")
    .map_err(|e| Error::Other(e.to_string()))?;

  let (_vm, heap) = realm.vm_and_heap_mut();
  assert_eq!(get_string(heap, value), "UTF-8|UTF-8|UTF-8");
  Ok(())
}

#[test]
fn document_title_is_exposed_and_writable() -> Result<()> {
  let url = "https://example.com/";
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new(url)).map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script(
      "document.title === '' && (document.title = 'hello') && document.title === 'hello'",
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn pop_state_event_and_hash_change_event_constructors_are_exposed() -> Result<()> {
  let url = "https://example.com/";
  let mut realm = WindowRealm::new_with_js_execution_options(WindowRealmConfig::new(url), js_opts_for_test())
    .map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script("typeof PopStateEvent === 'function' && typeof HashChangeEvent === 'function'")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));

  let ok = realm
    .exec_script("new PopStateEvent('popstate', { state: 123 }).state === 123")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));

  let ok = realm
    .exec_script("new PopStateEvent('popstate').state === null")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));

  let ok = realm
    .exec_script("(function () { let ev = new PopStateEvent('popstate', { state: 1 }); return ev instanceof PopStateEvent && ev instanceof Event; })()")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));

  let ok = realm
    .exec_script("new HashChangeEvent('hashchange', { oldURL: 'a', newURL: 'b' }).oldURL === 'a' && new HashChangeEvent('hashchange', { oldURL: 'a', newURL: 'b' }).newURL === 'b'")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));

  let ok = realm
    .exec_script("new HashChangeEvent('hashchange').oldURL === '' && new HashChangeEvent('hashchange').newURL === ''")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));

  let ok = realm
    .exec_script("document.createEvent('PopStateEvent') instanceof PopStateEvent && document.createEvent('HashChangeEvent') instanceof HashChangeEvent")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));

  let ok = realm
    .exec_script("Object.getPrototypeOf(PopStateEvent.prototype) === Event.prototype && Object.getPrototypeOf(HashChangeEvent.prototype) === Event.prototype")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));

  let ok = realm
    .exec_script("(function () { 'use strict'; let e = new PopStateEvent('popstate', { state: 1 }); try { e.state = 2; } catch (_) {} return e.state === 1; })()")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));

  Ok(())
}

#[test]
fn location_fragment_navigation_emits_hashchange_without_event_loop() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  // Same-document fragment navigation should queue `hashchange` asynchronously. When driving
  // `WindowRealm` directly (no HTML event loop installed), the event should not fire during the
  // same script turn.
  let len = realm
    .exec_script(
      r#"
      globalThis.__events = [];
      window.addEventListener('hashchange', (e) => {
        __events.push([e.oldURL, e.newURL, (typeof HashChangeEvent === 'function') && (e instanceof HashChangeEvent)]);
      });
      location.href = '#a';
      __events.length
      "#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(len, Value::Number(0.0));

  realm
    .perform_microtask_checkpoint()
    .map_err(|e| Error::Other(e.to_string()))?;

  let events = realm
    .exec_script("JSON.stringify(__events)")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(
    get_string(realm.heap(), events),
    r#"[["https://example.com/","https://example.com/#a",true]]"#
  );
  Ok(())
}

#[test]
fn document_current_script_tracks_sequential_classic_scripts() -> Result<()> {
  #[derive(Default)]
  struct NoopHostHooks;

  impl VmHostHooks for NoopHostHooks {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
  }

  #[derive(Default)]
  struct RecordingExecutor {
    observed: Vec<usize>,
  }

  impl ScriptBlockExecutor<WindowHostState> for RecordingExecutor {
    fn execute_script(
      &mut self,
      host: &mut WindowHostState,
      _orchestrator: &mut ScriptOrchestrator,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      let (vm_host, realm) = host.vm_host_and_window_realm()?;
      let mut hooks = NoopHostHooks::default();
      let value = realm
        .exec_script_with_host_and_hooks(
          vm_host,
          &mut hooks,
          "document.currentScript.__fastrender_node_id",
        )
        .map_err(|e| Error::Other(e.to_string()))?;
      let Value::Number(n) = value else {
        return Err(Error::Other(
          "expected document.currentScript.__fastrender_node_id to be a number".to_string(),
        ));
      };
      let as_usize = n as usize;
      if (as_usize as f64) != n {
        return Err(Error::Other(format!(
          "expected document.currentScript.__fastrender_node_id to be an integer, got {n:?}"
        )));
      }
      self.observed.push(as_usize);
      Ok(())
    }
  }

  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><script></script><script></script>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let scripts = find_script_elements(host.dom());
  assert_eq!(scripts.len(), 2);

  let mut orchestrator = ScriptOrchestrator::new();
  let mut executor = RecordingExecutor::default();

  // Outside execution, currentScript should be null.
  {
    let (vm_host, realm) = host.vm_host_and_window_realm()?;
    let mut hooks = NoopHostHooks::default();
    let value = realm
      .exec_script_with_host_and_hooks(vm_host, &mut hooks, "document.currentScript")
      .map_err(|e| Error::Other(e.to_string()))?;
    assert_eq!(value, Value::Null);
  }

  orchestrator.execute_script_element(&mut host, scripts[0], ScriptType::Classic, &mut executor)?;
  {
    let (vm_host, realm) = host.vm_host_and_window_realm()?;
    let mut hooks = NoopHostHooks::default();
    let value = realm
      .exec_script_with_host_and_hooks(vm_host, &mut hooks, "document.currentScript")
      .map_err(|e| Error::Other(e.to_string()))?;
    assert_eq!(value, Value::Null);
  }

  orchestrator.execute_script_element(&mut host, scripts[1], ScriptType::Classic, &mut executor)?;
  {
    let (vm_host, realm) = host.vm_host_and_window_realm()?;
    let mut hooks = NoopHostHooks::default();
    let value = realm
      .exec_script_with_host_and_hooks(vm_host, &mut hooks, "document.currentScript")
      .map_err(|e| Error::Other(e.to_string()))?;
    assert_eq!(value, Value::Null);
  }

  assert_eq!(
    executor.observed,
    vec![scripts[0].index(), scripts[1].index()]
  );
  Ok(())
}

#[test]
fn location_href_setter_requests_navigation_and_interrupts() -> Result<()> {
  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.com/"),
    js_opts_for_test(),
  )
  .map_err(|e| Error::Other(e.to_string()))?;

  let err = {
    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();

    let location_key_s = scope
      .alloc_string("location")
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .push_root(Value::String(location_key_s))
      .map_err(|e| Error::Other(e.to_string()))?;
    let location_key = PropertyKey::from_string(location_key_s);
    let location = vm
      .get(&mut scope, global, location_key)
      .map_err(|e| Error::Other(e.to_string()))?;
    let Value::Object(location_obj) = location else {
      panic!("expected location to be an object");
    };

    let href_key_s = scope
      .alloc_string("href")
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .push_root(Value::String(href_key_s))
      .map_err(|e| Error::Other(e.to_string()))?;
    let href_key = PropertyKey::from_string(href_key_s);

    let new_url_s = scope
      .alloc_string("https://example.com/next")
      .map_err(|e| Error::Other(e.to_string()))?;
    let new_value = Value::String(new_url_s);

    scope
      .ordinary_set(
        vm,
        location_obj,
        href_key,
        new_value,
        Value::Object(location_obj),
      )
      .expect_err("expected location.href setter to interrupt execution")
  };
  assert!(
    matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
    "unexpected error: {err:?}"
  );

  // Reset the interrupt flag so subsequent scripts could run in this realm if desired.
  realm.reset_interrupt();

  let req = realm
    .take_pending_navigation_request()
    .expect("expected pending navigation request");
  assert_eq!(req.url, "https://example.com/next");
  assert!(!req.replace);
  assert!(realm.take_pending_navigation_request().is_none());
  Ok(())
}

#[test]
fn location_pathname_setter_requests_navigation_and_interrupts() -> Result<()> {
  for source in ["location.pathname = '/next'", "location.pathname = 'next'"] {
    let mut realm =
      WindowRealm::new_with_js_execution_options(WindowRealmConfig::new("https://example.com/"), js_opts_for_test())
      .map_err(|e| Error::Other(e.to_string()))?;

    let err = realm
      .exec_script(source)
      .expect_err("expected location.pathname setter to interrupt execution");
    assert!(
      matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
      "unexpected error: {err:?}"
    );
    realm.reset_interrupt();

    let req = realm
      .take_pending_navigation_request()
      .expect("expected pending navigation request");
    assert_eq!(req.url, "https://example.com/next");
    assert!(!req.replace);
    assert!(realm.take_pending_navigation_request().is_none());
  }
  Ok(())
}

#[test]
fn location_search_setter_requests_navigation_and_interrupts() -> Result<()> {
  for source in ["location.search = '?q=1'", "location.search = 'q=1'"] {
    let mut realm =
      WindowRealm::new_with_js_execution_options(WindowRealmConfig::new("https://example.com/"), js_opts_for_test())
      .map_err(|e| Error::Other(e.to_string()))?;

    let err = realm
      .exec_script(source)
      .expect_err("expected location.search setter to interrupt execution");
    assert!(
      matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
      "unexpected error: {err:?}"
    );
    realm.reset_interrupt();

    let req = realm
      .take_pending_navigation_request()
      .expect("expected pending navigation request");
    assert_eq!(req.url, "https://example.com/?q=1");
    assert!(!req.replace);
    assert!(realm.take_pending_navigation_request().is_none());
  }
  Ok(())
}

#[test]
fn location_assign_and_replace_request_navigation() -> Result<()> {
  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.com/"),
    js_opts_for_test(),
  )
  .map_err(|e| Error::Other(e.to_string()))?;

  let err = realm
    .exec_script("location.assign('/a')")
    .expect_err("expected assign() to interrupt execution");
  assert!(
    matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
    "unexpected error: {err:?}"
  );
  realm.reset_interrupt();
  let req = realm
    .take_pending_navigation_request()
    .expect("expected pending navigation request");
  assert_eq!(req.url, "https://example.com/a");
  assert!(!req.replace);

  let err = realm
    .exec_script("location.replace('/b')")
    .expect_err("expected replace() to interrupt execution");
  assert!(
    matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
    "unexpected error: {err:?}"
  );
  realm.reset_interrupt();
  let req = realm
    .take_pending_navigation_request()
    .expect("expected pending navigation request");
  assert_eq!(req.url, "https://example.com/b");
  assert!(req.replace);
  Ok(())
}

#[test]
fn document_location_aliases_window_location_for_normal_documents() -> Result<()> {
  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.com/"),
    js_opts_for_test(),
  )
  .map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script(
      "document.location === window.location && document.location.href === location.href",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn window_and_document_location_assignment_requests_navigation() -> Result<()> {
  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.com/"),
    js_opts_for_test(),
  )
  .map_err(|e| Error::Other(e.to_string()))?;

  let err = realm
    .exec_script("location = '/a'")
    .expect_err("expected assignment to interrupt execution");
  assert!(
    matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
    "unexpected error: {err:?}"
  );
  realm.reset_interrupt();
  let req = realm
    .take_pending_navigation_request()
    .expect("expected pending navigation request");
  assert_eq!(req.url, "https://example.com/a");
  assert!(!req.replace);

  let err = realm
    .exec_script("window.location = '/b'")
    .expect_err("expected assignment to interrupt execution");
  assert!(
    matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
    "unexpected error: {err:?}"
  );
  realm.reset_interrupt();
  let req = realm
    .take_pending_navigation_request()
    .expect("expected pending navigation request");
  assert_eq!(req.url, "https://example.com/b");
  assert!(!req.replace);

  let err = realm
    .exec_script("document.location = '/c'")
    .expect_err("expected assignment to interrupt execution");
  assert!(
    matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
    "unexpected error: {err:?}"
  );
  realm.reset_interrupt();
  let req = realm
    .take_pending_navigation_request()
    .expect("expected pending navigation request");
  assert_eq!(req.url, "https://example.com/c");
  assert!(!req.replace);

  Ok(())
}

#[test]
fn document_location_fragment_assignment_updates_url_without_navigation_request_and_fires_hashchange(
) -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;

  let value = host.exec_script(
    r#"
globalThis.__hashchange_count = 0;
globalThis.__hashchange_oldURL = '';
globalThis.__hashchange_newURL = '';
window.addEventListener('hashchange', (e) => {
  globalThis.__hashchange_count++;
  globalThis.__hashchange_oldURL = e.oldURL;
  globalThis.__hashchange_newURL = e.newURL;
});

const beforeHref = location.href;
const beforeLen = history.length;
document.location = '#a';
const afterHref = location.href;
const afterLen = history.length;

// Hashchange must be queued as a task (not fired synchronously).
const firedSync = globalThis.__hashchange_count;
[beforeHref, afterHref, beforeLen, afterLen, firedSync].join('|')
"#,
  )?;

  assert_eq!(
    get_string(host.host().window().heap(), value),
    "https://example.com/|https://example.com/#a|1|2|0"
  );
  assert!(
    host
      .host_mut()
      .window_mut()
      .take_pending_navigation_request()
      .is_none(),
    "fragment navigation must not create a pending navigation request"
  );

  assert_eq!(
    host.run_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let value = host.exec_script(
    "globalThis.__hashchange_count + '|' + globalThis.__hashchange_oldURL + '|' + globalThis.__hashchange_newURL",
  )?;
  assert_eq!(
    get_string(host.host().window().heap(), value),
    "1|https://example.com/|https://example.com/#a"
  );
  Ok(())
}

#[test]
fn location_stringification_matches_href() -> Result<()> {
  let url = "https://example.com/path?query#hash";
  let mut realm = WindowRealm::new(WindowRealmConfig::new(url)).map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script("String(location) === location.href && (location + '') === location.href && location.toString() === location.href && location.toJSON() === location.href")
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn string_locale_methods_exist_and_behave() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script(
      r#"(function () {
  return (
    typeof ''.toLocaleLowerCase === 'function' &&
    typeof ''.toLocaleUpperCase === 'function' &&
    typeof ''.localeCompare === 'function' &&
    'A'.toLocaleLowerCase() === 'a' &&
    'a'.toLocaleUpperCase() === 'A' &&
    'a'.localeCompare('a') === 0 &&
    'a'.localeCompare('b') < 0 &&
    'b'.localeCompare('a') > 0
  );
})()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn history_and_location_constructors_exist() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script(
      "typeof History === 'function' &&\n\
       typeof History.prototype.pushState === 'function' &&\n\
       typeof History.prototype.replaceState === 'function' &&\n\
       typeof History.prototype.back === 'function' &&\n\
       typeof History.prototype.forward === 'function' &&\n\
       typeof History.prototype.go === 'function' &&\n\
       history.pushState === History.prototype.pushState &&\n\
       history.replaceState === History.prototype.replaceState &&\n\
       history.back === History.prototype.back &&\n\
       history.forward === History.prototype.forward &&\n\
       history.go === History.prototype.go &&\n\
       history instanceof History &&\n\
       history.constructor === History &&\n\
       typeof Location === 'function' &&\n\
       typeof Location.prototype.assign === 'function' &&\n\
       typeof Location.prototype.replace === 'function' &&\n\
       typeof Location.prototype.reload === 'function' &&\n\
       typeof Location.prototype.toString === 'function' &&\n\
       location.assign === Location.prototype.assign &&\n\
       location.replace === Location.prototype.replace &&\n\
       location.reload === Location.prototype.reload &&\n\
       location.toString === Location.prototype.toString &&\n\
       location instanceof Location &&\n\
       location.constructor === Location",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn history_and_location_methods_throw_illegal_invocation_on_wrong_receiver() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let err = realm
    .exec_script(
      r#"(() => {
        const f = history.pushState;
        try {
          f.call({}, 1, '', '#a');
          return 'no';
        } catch (e) {
          return e.name + '|' + e.message;
        }
      })()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), err), "TypeError|Illegal invocation");

  let err = realm
    .exec_script(
      r#"(() => {
        const f = history.back;
        try {
          f.call({});
          return 'no';
        } catch (e) {
          return e.name + '|' + e.message;
        }
      })()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), err), "TypeError|Illegal invocation");

  let err = realm
    .exec_script(
      r#"(() => {
        const f = location.assign;
        try {
          f.call({}, '#a');
          return 'no';
        } catch (e) {
          return e.name + '|' + e.message;
        }
      })()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), err), "TypeError|Illegal invocation");
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "location.assign illegal invocation must not schedule navigation"
  );

  let err = realm
    .exec_script(
      r#"(() => {
        const set = Object.getOwnPropertyDescriptor(location, 'href').set;
        try {
          set.call({}, 'https://x/');
          return 'no';
        } catch (e) {
          return e.name + '|' + e.message;
        }
      })()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), err), "TypeError|Illegal invocation");
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "location.href illegal invocation must not schedule navigation"
  );

  Ok(())
}

#[test]
fn history_push_state_updates_location_without_navigation() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script("history.pushState(null, '', '/next'); location.href + '|' + document.URL + '|' + document.baseURI")
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(
    get_string(realm.heap(), value),
    "https://example.com/next|https://example.com/next|https://example.com/next"
  );
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "history.pushState should not schedule navigation"
  );
  Ok(())
}

#[test]
fn document_url_is_read_only_and_tracks_location_href() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script(
      r#"(function () {
  if (document.URL !== location.href) return false;
  history.pushState(null, '', '#a');
  if (document.URL !== location.href) return false;

  const before = document.URL;
  document.URL = 'https://evil/';
  if (document.URL !== before) return false;

  let strictOk = true;
  try {
    (function () { 'use strict'; document.URL = 'https://evil2/'; })();
  } catch (e) {
    strictOk = e instanceof TypeError;
  }
  if (document.URL !== before) return false;
  return strictOk;
})()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn history_push_state_clones_objects() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script(
      r#"(function () {
  const s = { a: 1 };
  history.pushState(s, '');
  s.a = 2;
  return history.state.a === 1 && history.state !== s;
})()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn history_push_state_preserves_cycles() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script(
      r#"(function () {
  const a = {};
  a.self = a;
  history.pushState(a, '');
  return history.state.self === history.state;
})()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn history_push_state_throws_data_clone_error_and_does_not_update_url() -> Result<()> {
  let url = "https://example.com/original";
  let mut realm = WindowRealm::new(WindowRealmConfig::new(url)).map_err(|e| Error::Other(e.to_string()))?;

  let before = realm
    .exec_script("location.href + '|' + document.URL + '|' + (history.state === null)")
    .map_err(|e| Error::Other(e.to_string()))?;
  let before_s = get_string(realm.heap(), before);
  assert_eq!(before_s, format!("{url}|{url}|true"));

  let err = realm
    .exec_script("history.pushState(function () {}, '', '/next')")
    .expect_err("expected DataCloneError");
  let err_msg = {
    let (_vm, heap) = realm.vm_and_heap_mut();
    format_vm_error(heap, err)
  };
  assert_eq!(err_msg, "DataCloneError");

  let after = realm
    .exec_script("location.href + '|' + document.URL + '|' + (history.state === null)")
    .map_err(|e| Error::Other(e.to_string()))?;
  let after_s = get_string(realm.heap(), after);
  assert_eq!(after_s, before_s, "URL/state must not change after DataCloneError");

  let err = realm
    .exec_script("history.pushState({ f: function () {} }, '', '/next2')")
    .expect_err("expected DataCloneError");
  let err_msg = {
    let (_vm, heap) = realm.vm_and_heap_mut();
    format_vm_error(heap, err)
  };
  assert_eq!(err_msg, "DataCloneError");

  let after2 = realm
    .exec_script("location.href + '|' + document.URL + '|' + (history.state === null)")
    .map_err(|e| Error::Other(e.to_string()))?;
  let after2_s = get_string(realm.heap(), after2);
  assert_eq!(
    after2_s, before_s,
    "URL/state must not change after nested DataCloneError"
  );

  Ok(())
}

#[test]
fn history_go_zero_requests_reload_and_interrupts() -> Result<()> {
  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.com/"),
    js_opts_for_test(),
  )
  .map_err(|e| Error::Other(e.to_string()))?;

  // Ensure we reload the *current* document URL (which can be updated by pushState).
  realm
    .exec_script("history.pushState(null, '', '/next')")
    .map_err(|e| Error::Other(e.to_string()))?;

  let err = realm
    .exec_script("history.go(0)")
    .expect_err("expected history.go(0) to interrupt execution");
  assert!(
    matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
    "unexpected error: {err:?}"
  );
  realm.reset_interrupt();
  let req = realm
    .take_pending_navigation_request()
    .expect("expected pending navigation request");
  assert_eq!(req.url, "https://example.com/next");
  assert!(req.replace);

  let err = realm
    .exec_script("history.go()")
    .expect_err("expected history.go() to interrupt execution");
  assert!(
    matches!(err, VmError::Termination(ref term) if term.reason == TerminationReason::Interrupted),
    "unexpected error: {err:?}"
  );
  realm.reset_interrupt();
  let req = realm
    .take_pending_navigation_request()
    .expect("expected pending navigation request");
  assert_eq!(req.url, "https://example.com/next");
  assert!(req.replace);

  Ok(())
}

#[test]
fn history_push_replace_state_null_url_does_not_change_document_url() -> Result<()> {
  let start_url = "https://example.com/start";

  let mut realm = WindowRealm::new(WindowRealmConfig::new(start_url))
    .map_err(|e| Error::Other(e.to_string()))?;
  let value = realm
    .exec_script("history.pushState({}, '', null); location.href + '|' + document.URL")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), format!("{start_url}|{start_url}"));

  let mut realm = WindowRealm::new(WindowRealmConfig::new(start_url))
    .map_err(|e| Error::Other(e.to_string()))?;
  let value = realm
    .exec_script("history.replaceState({}, '', null); location.href + '|' + document.URL")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), format!("{start_url}|{start_url}"));

  Ok(())
}

#[test]
fn history_push_state_null_url_updates_length_and_state_without_changing_href() -> Result<()> {
  let start_url = "https://example.com/";
  let mut realm = WindowRealm::new(WindowRealmConfig::new(start_url))
    .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script(
      r#"
      const before = location.href;
      history.pushState({a: 1}, '', null);
      const after = location.href;
      [before, after, history.length, history.state && history.state.a].join('|')
      "#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(
    get_string(realm.heap(), value),
    format!("{start_url}|{start_url}|2|1")
  );
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "history.pushState with null URL should not schedule navigation"
  );
  Ok(())
}

#[test]
fn history_push_and_replace_state_update_length_and_state() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script(
      r#"
      const out = [];
      out.push(history.length);
      history.pushState(1, '', null);
      out.push(history.length, history.state);
      history.pushState(2, '');
      out.push(history.length, history.state);
      history.replaceState(3, '', null);
      out.push(history.length, history.state);
      out.join('|')
      "#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(get_string(realm.heap(), value), "1|2|1|3|2|3|3");
  Ok(())
}

#[test]
fn history_push_state_null_url_is_ignored() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/start"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script("history.pushState(1, '', null); location.href + '|' + document.URL + '|' + document.baseURI")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(
    get_string(realm.heap(), value),
    "https://example.com/start|https://example.com/start|https://example.com/start"
  );
  Ok(())
}

#[test]
fn history_state_change_cross_origin_throws_security_error() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let before = realm
    .exec_script(
      "location.href + '|' + document.URL + '|' + document.baseURI + '|' + history.length + '|' + (history.state === null)",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  let before_s = get_string(realm.heap(), before);

  let value = realm
    .exec_script("try { history.pushState({}, '', 'https://evil.com/'); } catch(e) { e && e.name; }")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), "SecurityError");

  let after = realm
    .exec_script(
      "location.href + '|' + document.URL + '|' + document.baseURI + '|' + history.length + '|' + (history.state === null)",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(
    get_string(realm.heap(), after),
    before_s,
    "history.pushState must not mutate URL/state/length after SecurityError"
  );
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "history.pushState failure should not schedule navigation"
  );

  let value = realm
    .exec_script("try { history.replaceState({}, '', 'https://evil.com/'); } catch(e) { e && e.name; }")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), "SecurityError");

  let after = realm
    .exec_script(
      "location.href + '|' + document.URL + '|' + document.baseURI + '|' + history.length + '|' + (history.state === null)",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(
    get_string(realm.heap(), after),
    before_s,
    "history.replaceState must not mutate URL/state/length after SecurityError"
  );
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "history.replaceState failure should not schedule navigation"
  );
  Ok(())
}

#[test]
fn history_push_state_cross_origin_url_throws_security_error_and_does_not_mutate() -> Result<()> {
  let url = "https://example.com/path";
  let mut realm =
    WindowRealm::new_with_js_execution_options(WindowRealmConfig::new(url), js_opts_for_test())
      .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script(
      r#"(() => {
  const beforeHref = location.href;
  const beforeLen = history.length;
  const beforeState = history.state;
  let err = null;
  try {
    history.pushState({a: 1}, '', 'https://other.example/');
  } catch (e) {
    err = { name: e && e.name, message: e && e.message };
  }
  const afterHref = location.href;
  const afterLen = history.length;
  const afterState = history.state;
  return JSON.stringify({ beforeHref, beforeLen, beforeState, afterHref, afterLen, afterState, err });
})()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  let out_s = get_string(realm.heap(), value);
  let out: serde_json::Value =
    serde_json::from_str(&out_s).map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(out["beforeHref"], url);
  assert_eq!(out["err"]["name"], "SecurityError");
  assert_eq!(out["afterHref"], out["beforeHref"]);
  assert_eq!(out["afterLen"], out["beforeLen"]);
  assert_eq!(out["afterState"], out["beforeState"]);
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "history.pushState failure should not schedule navigation"
  );

  Ok(())
}

#[test]
fn history_replace_state_cross_origin_url_throws_security_error_and_does_not_mutate() -> Result<()> {
  let url = "https://example.com/path";
  let mut realm =
    WindowRealm::new_with_js_execution_options(WindowRealmConfig::new(url), js_opts_for_test())
      .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script(
      r#"(() => {
  const beforeHref = location.href;
  const beforeLen = history.length;
  const beforeState = history.state;
  let err = null;
  try {
    history.replaceState({a: 1}, '', 'https://other.example/');
  } catch (e) {
    err = { name: e && e.name, message: e && e.message };
  }
  const afterHref = location.href;
  const afterLen = history.length;
  const afterState = history.state;
  return JSON.stringify({ beforeHref, beforeLen, beforeState, afterHref, afterLen, afterState, err });
})()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  let out_s = get_string(realm.heap(), value);
  let out: serde_json::Value =
    serde_json::from_str(&out_s).map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(out["beforeHref"], url);
  assert_eq!(out["err"]["name"], "SecurityError");
  assert_eq!(out["afterHref"], out["beforeHref"]);
  assert_eq!(out["afterLen"], out["beforeLen"]);
  assert_eq!(out["afterState"], out["beforeState"]);
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "history.replaceState failure should not schedule navigation"
  );

  Ok(())
}

#[test]
fn history_push_state_opaque_origin_requires_stable_scheme() -> Result<()> {
  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("about:blank"),
    js_opts_for_test(),
  )
  .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script(
      r#"(() => {
  const beforeHref = location.href;
  const beforeLen = history.length;
  const beforeState = history.state;
  const beforeOrigin = location.origin;
  let err = null;
  try {
    history.pushState({a: 1}, '', 'data:text/plain,x');
  } catch (e) {
    err = { name: e && e.name, message: e && e.message };
  }
  const afterHref = location.href;
  const afterLen = history.length;
  const afterState = history.state;
  return JSON.stringify({ beforeHref, beforeLen, beforeState, beforeOrigin, afterHref, afterLen, afterState, err });
})()"#,
    )
    .map_err(|e| Error::Other(e.to_string()))?;

  let out_s = get_string(realm.heap(), value);
  let out: serde_json::Value =
    serde_json::from_str(&out_s).map_err(|e| Error::Other(e.to_string()))?;

  assert_eq!(out["beforeOrigin"], "null");
  assert_eq!(out["err"]["name"], "SecurityError");
  assert_eq!(out["afterHref"], out["beforeHref"]);
  assert_eq!(out["afterLen"], out["beforeLen"]);
  assert_eq!(out["afterState"], out["beforeState"]);
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "history.pushState failure should not schedule navigation"
  );

  Ok(())
}

#[test]
fn history_state_change_invalid_url_throws_security_error() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let before = realm
    .exec_script(
      "location.href + '|' + document.URL + '|' + document.baseURI + '|' + history.length + '|' + (history.state === null)",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  let before_s = get_string(realm.heap(), before);

  let value = realm
    .exec_script("try { history.pushState({}, '', 'http://'); } catch(e) { e && e.name; }")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), "SecurityError");

  let after = realm
    .exec_script(
      "location.href + '|' + document.URL + '|' + document.baseURI + '|' + history.length + '|' + (history.state === null)",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(
    get_string(realm.heap(), after),
    before_s,
    "history.pushState must not mutate URL/state/length after SecurityError"
  );
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "history.pushState failure should not schedule navigation"
  );

  let value = realm
    .exec_script("try { history.replaceState({}, '', 'http://'); } catch(e) { e && e.name; }")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), "SecurityError");

  let after = realm
    .exec_script(
      "location.href + '|' + document.URL + '|' + document.baseURI + '|' + history.length + '|' + (history.state === null)",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(
    get_string(realm.heap(), after),
    before_s,
    "history.replaceState must not mutate URL/state/length after SecurityError"
  );
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "history.replaceState failure should not schedule navigation"
  );
  Ok(())
}

#[test]
fn js_execution_can_observe_window_globals() -> Result<()> {
  let url = "https://example.com/path";
  let mut realm =
    WindowRealm::new_with_js_execution_options(WindowRealmConfig::new(url), js_opts_for_test())
      .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script("window === globalThis && self === window && top === window && parent === window")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(value, Value::Bool(true));

  let value = realm
    .exec_script("document.URL")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), url);

  let value = realm
    .exec_script("location.href")
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(get_string(realm.heap(), value), url);
  Ok(())
}

#[test]
fn location_stringifier_returns_href() -> Result<()> {
  let url = "https://example.com/path?x=1#y";
  let mut realm = WindowRealm::new_with_js_execution_options(WindowRealmConfig::new(url), js_opts_for_test())
    .map_err(|e| Error::Other(e.to_string()))?;

  let ok = realm
    .exec_script(
      "String(location) === location.href && '' + location === location.href && location.toString() === location.href",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn event_loop_microtask_checkpoint_uses_dom_shim_hooks() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head></head><body><div id=t></div></body></html>",
  )?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
 const el = document.getElementById('t');
 Promise.resolve().then(() => { el.dataset.fooBar = 'baz'; });
 "#,
  )?;

  let before = host.exec_script_in_event_loop(
    &mut event_loop,
    "document.getElementById('t').getAttribute('data-foo-bar') === null",
  )?;
  assert_eq!(before, Value::Bool(true));

  event_loop.perform_microtask_checkpoint(&mut host)?;

  let after = host.exec_script_in_event_loop(
    &mut event_loop,
    "document.getElementById('t').getAttribute('data-foo-bar') === 'baz'",
  )?;
  assert_eq!(after, Value::Bool(true));
  Ok(())
}

#[test]
fn html_element_instanceof_uses_dom_platform_prototypes() -> Result<()> {
  let dom = fastrender::dom2::parse_html(
    "<!doctype html><html><body>\n\
      <input id=\"i\">\n\
      <div id=\"d\"></div>\n\
      <svg id=\"s\"></svg>\n\
    </body></html>",
  )?;
  let mut host = make_host(dom, "https://example.com/")?;
  let ok = host.exec_script(
    "(() => {\n\
      const input = document.getElementById('i');\n\
      const div = document.getElementById('d');\n\
      const svg = document.getElementById('s');\n\
\n\
      if (typeof HTMLElement !== 'function') return false;\n\
      if (typeof HTMLInputElement !== 'function') return false;\n\
      if (typeof HTMLDivElement !== 'function') return false;\n\
\n\
      if (!(input instanceof HTMLInputElement)) return false;\n\
      if (!(input instanceof HTMLElement)) return false;\n\
      if (!(div instanceof HTMLDivElement)) return false;\n\
      if (!(div instanceof HTMLElement)) return false;\n\
      if (svg instanceof HTMLElement) return false;\n\
      if (!(svg instanceof Element)) return false;\n\
\n\
      if (HTMLElement.prototype === Element.prototype) return false;\n\
      if (Object.getPrototypeOf(HTMLElement.prototype) !== Element.prototype) return false;\n\
      if (Object.getPrototypeOf(HTMLInputElement.prototype) !== HTMLElement.prototype) return false;\n\
      return true;\n\
    })()",
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn shadow_root_is_distinct_interface_with_core_attributes() -> Result<()> {
  let dom = fastrender::dom2::parse_html(
    "<!doctype html><html><body>\n\
      <div id=\"open\"></div>\n\
      <div id=\"closed\"></div>\n\
    </body></html>",
  )?;
  let mut host = make_host(dom, "https://example.com/")?;
  let ok = host.exec_script(
    "(() => {\n\
      if (typeof ShadowRoot !== 'function') return false;\n\
\n\
      const openHost = document.getElementById('open');\n\
      const closedHost = document.getElementById('closed');\n\
\n\
      const openSr = openHost.attachShadow({ mode: 'open', delegatesFocus: true, slotAssignment: 'manual' });\n\
      const closedSr = closedHost.attachShadow({ mode: 'closed' });\n\
\n\
      // Shadow roots must not appear in light DOM traversal.\n\
      const openLight = document.createElement('span');\n\
      openHost.appendChild(openLight);\n\
      if (openHost.firstChild !== openLight) return false;\n\
      if (openHost.lastChild !== openLight) return false;\n\
      if (openHost.childNodes.length !== 1) return false;\n\
      if (openHost.childNodes[0] !== openLight) return false;\n\
      if (openLight.previousSibling !== null) return false;\n\
      if (openLight.nextSibling !== null) return false;\n\
      if (openSr.parentNode !== null) return false;\n\
      if (openSr.previousSibling !== null) return false;\n\
      if (openSr.nextSibling !== null) return false;\n\
\n\
      const closedLight = document.createElement('span');\n\
      closedHost.appendChild(closedLight);\n\
      if (closedHost.firstChild !== closedLight) return false;\n\
      if (closedHost.childNodes.length !== 1) return false;\n\
      if (closedLight.previousSibling !== null) return false;\n\
      if (closedSr.parentNode !== null) return false;\n\
      if (closedSr.previousSibling !== null) return false;\n\
      if (closedSr.nextSibling !== null) return false;\n\
\n\
      if (!(openSr instanceof ShadowRoot)) return false;\n\
      if (!(openSr instanceof DocumentFragment)) return false;\n\
\n\
      if (Object.getPrototypeOf(ShadowRoot) !== DocumentFragment) return false;\n\
      if (Object.getPrototypeOf(ShadowRoot.prototype) !== DocumentFragment.prototype) return false;\n\
\n\
      if (openSr.host !== openHost) return false;\n\
      if (openSr.mode !== 'open') return false;\n\
      if (openSr.delegatesFocus !== true) return false;\n\
      if (openSr.slotAssignment !== 'manual') return false;\n\
      if (openHost.shadowRoot !== openSr) return false;\n\
\n\
      if (closedSr.host !== closedHost) return false;\n\
      if (closedSr.mode !== 'closed') return false;\n\
      if (closedSr.delegatesFocus !== false) return false;\n\
      if (closedSr.slotAssignment !== 'named') return false;\n\
      if (closedHost.shadowRoot !== null) return false;\n\
\n\
      return true;\n\
    })()",
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn shadow_root_works_in_webidl_dom_backend() -> Result<()> {
  let dom = fastrender::dom2::parse_html(
    "<!doctype html><html><body>\n\
      <div id=\"open\"></div>\n\
      <div id=\"closed\"></div>\n\
    </body></html>",
  )?;

  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock.clone());
  let mut host = WindowHostState::new_with_fetcher_and_clock_and_options_and_dom_backend(
    dom,
    "https://example.com/",
    Arc::new(InMemoryFetcher::default()),
    clock,
    js_opts_for_test(),
    DomBindingsBackend::WebIdl,
  )?;

  let ok = host.exec_script_in_event_loop(
    &mut event_loop,
    "(() => {\n\
      if (typeof ShadowRoot !== 'function') return false;\n\
\n\
      const openHost = document.getElementById('open');\n\
      const closedHost = document.getElementById('closed');\n\
\n\
      const openSr = openHost.attachShadow({ mode: 'open', delegatesFocus: true, slotAssignment: 'manual' });\n\
      const closedSr = closedHost.attachShadow({ mode: 'closed' });\n\
\n\
      // Shadow roots must not appear in light DOM traversal.\n\
      const openLight = document.createElement('span');\n\
      openHost.appendChild(openLight);\n\
      if (openHost.firstChild !== openLight) return false;\n\
      if (openHost.lastChild !== openLight) return false;\n\
      if (openHost.childNodes.length !== 1) return false;\n\
      if (openHost.childNodes[0] !== openLight) return false;\n\
      if (openLight.previousSibling !== null) return false;\n\
      if (openLight.nextSibling !== null) return false;\n\
      if (openSr.parentNode !== null) return false;\n\
      if (openSr.previousSibling !== null) return false;\n\
      if (openSr.nextSibling !== null) return false;\n\
\n\
      const closedLight = document.createElement('span');\n\
      closedHost.appendChild(closedLight);\n\
      if (closedHost.firstChild !== closedLight) return false;\n\
      if (closedHost.childNodes.length !== 1) return false;\n\
      if (closedLight.previousSibling !== null) return false;\n\
      if (closedSr.parentNode !== null) return false;\n\
      if (closedSr.previousSibling !== null) return false;\n\
      if (closedSr.nextSibling !== null) return false;\n\
\n\
      if (!(openSr instanceof ShadowRoot)) return false;\n\
      if (!(openSr instanceof DocumentFragment)) return false;\n\
\n\
      if (Object.getPrototypeOf(ShadowRoot) !== DocumentFragment) return false;\n\
      if (Object.getPrototypeOf(ShadowRoot.prototype) !== DocumentFragment.prototype) return false;\n\
\n\
      if (openSr.host !== openHost) return false;\n\
      if (openSr.mode !== 'open') return false;\n\
      if (openSr.delegatesFocus !== true) return false;\n\
      if (openSr.slotAssignment !== 'manual') return false;\n\
      if (openHost.shadowRoot !== openSr) return false;\n\
\n\
      if (closedSr.host !== closedHost) return false;\n\
      if (closedSr.mode !== 'closed') return false;\n\
      if (closedSr.delegatesFocus !== false) return false;\n\
      if (closedSr.slotAssignment !== 'named') return false;\n\
      if (closedHost.shadowRoot !== null) return false;\n\
\n\
      return true;\n\
    })()",
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn strict_script_top_level_this_is_window() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
"use strict";
globalThis.__strict_this_ok = (this === window) && (this === globalThis);
"#,
  )?;

  let strict_this_ok = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    get_data_prop(&mut scope, global, "__strict_this_ok")
  };
  assert_eq!(strict_this_ok, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_jobs_and_queue_microtask_preserve_fifo_order() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__log = "";
Promise.resolve().then(() => { globalThis.__log += "p1,"; });
queueMicrotask(() => { globalThis.__log += "qm,"; });
Promise.resolve().then(() => { globalThis.__log += "p2,"; });
"#,
  )?;

  let before = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__log");
    get_string(scope.heap(), value)
  };
  assert_eq!(before, "");

  host.perform_microtask_checkpoint()?;

  let after = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__log");
    get_string(scope.heap(), value)
  };
  assert_eq!(after, "p1,qm,p2,");
  Ok(())
}

#[test]
fn named_scripts_route_promise_jobs_through_event_loop_microtasks() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHostState::new_with_fetcher_and_options(
    dom,
    "https://example.com/",
    Arc::new(InMemoryFetcher::default()),
    js_opts_for_test(),
  )?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_with_name_in_event_loop(
    &mut event_loop,
    "<test named script>",
    r#"
globalThis.__log = "";
Promise.resolve().then(() => { globalThis.__log += "p1,"; });
queueMicrotask(() => { globalThis.__log += "qm,"; });
Promise.resolve().then(() => { globalThis.__log += "p2,"; });
"#,
  )?;

  let before = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__log");
    get_string(scope.heap(), value)
  };
  assert_eq!(before, "");

  event_loop.perform_microtask_checkpoint(&mut host)?;

  let after = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__log");
    get_string(scope.heap(), value)
  };
  assert_eq!(after, "p1,qm,p2,");
  Ok(())
}

#[test]
fn promise_jobs_callbacks_can_mutate_dom() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
Promise.resolve().then(() => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'p';
  document.body.appendChild(d);
});
"#,
  )?;

  assert!(
    host.dom().get_element_by_id("p").is_none(),
    "element should not exist before the microtask checkpoint"
  );
  event_loop.perform_microtask_checkpoint(&mut host)?;
  assert!(
    host.dom().get_element_by_id("p").is_some(),
    "expected Promise job callback to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn queue_microtask_callbacks_can_mutate_dom() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
queueMicrotask(() => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'm';
  document.body.appendChild(d);
});
"#,
  )?;

  assert!(
    host.dom().get_element_by_id("m").is_none(),
    "element should not exist before the microtask checkpoint"
  );
  event_loop.perform_microtask_checkpoint(&mut host)?;
  assert!(
    host.dom().get_element_by_id("m").is_some(),
    "expected queueMicrotask callback to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn mutation_observer_callbacks_can_mutate_dom_and_receive_real_vm_host() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head></head><body><div id=target></div></body></html>",
  )?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  assert!(host.dom().get_element_by_id("mo").is_none());
  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
const target = document.getElementById('target');
const obs = new MutationObserver((_records, observer) => {
  observer.disconnect();
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'mo';
  document.body.appendChild(d);
});
obs.observe(target, { attributes: true });
target.setAttribute('data-x', '1');
"#,
  )?;

  assert!(
    host.dom().get_element_by_id("mo").is_none(),
    "element should not exist before the microtask checkpoint"
  );
  event_loop.perform_microtask_checkpoint(&mut host)?;
  assert!(
    host.dom().get_element_by_id("mo").is_some(),
    "expected MutationObserver callback to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn mutation_observer_records_are_webidl_shaped() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head></head><body><div id=target></div></body></html>",
  )?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__ok = false;
const target = document.getElementById('target');
const obs = new MutationObserver((records) => {
  const r = records[0];
  globalThis.__ok =
    (r instanceof MutationRecord) &&
    (r.addedNodes instanceof NodeList) &&
    (r.removedNodes instanceof NodeList) &&
    (typeof r.addedNodes.item === 'function') &&
    (typeof r.removedNodes.item === 'function') &&
    (r.type === 'attributes') &&
    (r.target === target) &&
    (r.attributeName === 'data-x');
});
obs.observe(target, { attributes: true });
target.setAttribute('data-x', '1');
"#,
  )?;

  event_loop.perform_microtask_checkpoint(&mut host)?;
  let ok = host.exec_script_in_event_loop(&mut event_loop, "globalThis.__ok")?;
  assert_eq!(ok, vm_js::Value::Bool(true));
  Ok(())
}

#[test]
fn set_timeout_callbacks_can_mutate_dom() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
setTimeout(() => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 't';
  document.body.appendChild(d);
}, 0);
"#,
  )?;

  assert!(
    host.dom().get_element_by_id("t").is_none(),
    "element should not exist before the event loop runs"
  );
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after firing the timeout"
  );
  assert!(
    host.dom().get_element_by_id("t").is_some(),
    "expected setTimeout callback to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn clear_timeout_prevents_callback_execution() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
const id = setTimeout(() => {
  const d = document.createElement('div');
  d.id = 'ct';
  document.body.appendChild(d);
}, 0);
clearTimeout(id);
"#,
  )?;

  assert!(host.dom().get_element_by_id("ct").is_none());
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after clearing the timeout"
  );
  assert!(
    host.dom().get_element_by_id("ct").is_none(),
    "expected clearTimeout to prevent callback execution"
  );
  Ok(())
}

#[test]
fn clear_interval_prevents_callback_execution() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
const id = setInterval(() => {
  const d = document.createElement('div');
  d.id = 'ci';
  document.body.appendChild(d);
}, 0);
clearInterval(id);
"#,
  )?;

  assert!(host.dom().get_element_by_id("ci").is_none());
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after clearing the interval"
  );
  assert!(
    host.dom().get_element_by_id("ci").is_none(),
    "expected clearInterval to prevent callback execution"
  );
  Ok(())
}

#[test]
fn set_timeout_callbacks_can_schedule_microtasks_that_mutate_dom() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
setTimeout(() => {
  queueMicrotask(() => {
    const d = document.createElement('div');
    d.id = 'tm';
    document.body.appendChild(d);
  });
}, 0);
"#,
  )?;

  assert!(
    host.dom().get_element_by_id("tm").is_none(),
    "element should not exist before the event loop runs"
  );
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after firing the timeout and draining microtasks"
  );
  assert!(
    host.dom().get_element_by_id("tm").is_some(),
    "expected microtask scheduled from setTimeout callback to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn set_interval_callbacks_can_mutate_dom_and_be_cleared() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
let count = 0;
const id = setInterval(() => {
  __fastrender_assert_vm_host();
  count++;
  if (count !== 1) return;
  clearInterval(id);
  const d = document.createElement('div');
  d.id = 'i';
  document.body.appendChild(d);
}, 0);
"#,
  )?;

  assert!(
    host.dom().get_element_by_id("i").is_none(),
    "element should not exist before the event loop runs"
  );
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after firing the interval once"
  );
  assert!(
    host.dom().get_element_by_id("i").is_some(),
    "expected setInterval callback to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn event_listener_callbacks_can_mutate_dom() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  assert!(host.dom().get_element_by_id("e").is_none());
  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
document.body.addEventListener('x', () => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'e';
  document.body.appendChild(d);
});
document.body.dispatchEvent(new Event('x'));
"#,
  )?;

  // `dispatchEvent` is synchronous, but run a checkpoint anyway to ensure any nested microtasks
  // don't affect assertions.
  event_loop.perform_microtask_checkpoint(&mut host)?;

  assert!(
    host.dom().get_element_by_id("e").is_some(),
    "expected EventTarget.dispatchEvent listener to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn dispatch_event_rejects_non_event_objects() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__threw = false;
globalThis.__name = '';
globalThis.__msg = '';
try {
  document.body.dispatchEvent({});
} catch (e) {
  globalThis.__threw = true;
  globalThis.__name = e.name;
  globalThis.__msg = e.message;
}
"#,
  )?;
  event_loop.perform_microtask_checkpoint(&mut host)?;

  let (threw, name, msg) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let threw = get_data_prop(&mut scope, global, "__threw");
    let name_value = get_data_prop(&mut scope, global, "__name");
    let msg_value = get_data_prop(&mut scope, global, "__msg");
    let heap = scope.heap();
    (threw, get_string(heap, name_value), get_string(heap, msg_value))
  };

  assert_eq!(threw, Value::Bool(true));
  assert_eq!(name, "TypeError");
  assert_eq!(msg, "EventTarget.dispatchEvent: event is not an Event");
  Ok(())
}

#[test]
fn custom_event_dispatch_preserves_detail() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__detail_before = null;
globalThis.__detail_after = null;
globalThis.__detail_in_listener = null;
globalThis.__is_custom = false;
globalThis.__dispatch_ret = false;

document.body.addEventListener('x', (e) => {
  globalThis.__detail_in_listener = e.detail;
  globalThis.__is_custom = (e instanceof CustomEvent);
});

const ev = new CustomEvent('x', { detail: 1 });
globalThis.__detail_before = ev.detail;
globalThis.__dispatch_ret = document.body.dispatchEvent(ev);
globalThis.__detail_after = ev.detail;
"#,
  )?;
  event_loop.perform_microtask_checkpoint(&mut host)?;

  let (detail_before, detail_after, detail_in_listener, is_custom, dispatch_ret) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    (
      get_data_prop(&mut scope, global, "__detail_before"),
      get_data_prop(&mut scope, global, "__detail_after"),
      get_data_prop(&mut scope, global, "__detail_in_listener"),
      get_data_prop(&mut scope, global, "__is_custom"),
      get_data_prop(&mut scope, global, "__dispatch_ret"),
    )
  };

  assert_eq!(detail_before, Value::Number(1.0));
  assert_eq!(detail_after, Value::Number(1.0));
  assert_eq!(detail_in_listener, Value::Number(1.0));
  assert_eq!(is_custom, Value::Bool(true));
  assert_eq!(dispatch_ret, Value::Bool(true));
  Ok(())
}

#[test]
fn event_brand_is_non_enumerable() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
const ev = new Event('x');
globalThis.__has_brand = (ev.__fastrender_event === true);
globalThis.__brand_in_keys = (Object.keys(ev).indexOf('__fastrender_event') !== -1);
globalThis.__kind_in_keys = (Object.keys(ev).indexOf('__fastrender_event_kind') !== -1);
globalThis.__kind = ev.__fastrender_event_kind;
"#,
  )?;

  let (has_brand, brand_in_keys, kind_in_keys, kind) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    (
      get_data_prop(&mut scope, global, "__has_brand"),
      get_data_prop(&mut scope, global, "__brand_in_keys"),
      get_data_prop(&mut scope, global, "__kind_in_keys"),
      get_data_prop(&mut scope, global, "__kind"),
    )
  };

  assert_eq!(has_brand, Value::Bool(true));
  assert_eq!(brand_in_keys, Value::Bool(false));
  assert_eq!(kind_in_keys, Value::Bool(false));
  // `BrandedEventKind::Event` must map to 0 (stable for downstream decoding).
  assert_eq!(kind, Value::Number(0.0));
  Ok(())
}

#[test]
fn mouse_event_is_branded_as_mouse_event_kind() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__kind = null;
globalThis.__detail_before = null;
globalThis.__detail_after = null;
globalThis.__detail_in_listener = null;
globalThis.__dispatch_ret = false;

document.body.addEventListener('x', (e) => {
  globalThis.__detail_in_listener = e.detail;
});

const ev = new MouseEvent('x', { detail: 3 });
globalThis.__kind = ev.__fastrender_event_kind;
globalThis.__detail_before = ev.detail;
globalThis.__dispatch_ret = document.body.dispatchEvent(ev);
globalThis.__detail_after = ev.detail;
"#,
  )?;
  event_loop.perform_microtask_checkpoint(&mut host)?;

  let (kind, detail_before, detail_after, detail_in_listener, dispatch_ret) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    (
      get_data_prop(&mut scope, global, "__kind"),
      get_data_prop(&mut scope, global, "__detail_before"),
      get_data_prop(&mut scope, global, "__detail_after"),
      get_data_prop(&mut scope, global, "__detail_in_listener"),
      get_data_prop(&mut scope, global, "__dispatch_ret"),
    )
  };

  // `BrandedEventKind::MouseEvent` must map to 7 (stable for downstream decoding).
  assert_eq!(kind, Value::Number(7.0));
  assert_eq!(detail_before, Value::Number(3.0));
  assert_eq!(detail_after, Value::Number(3.0));
  assert_eq!(detail_in_listener, Value::Number(3.0));
  assert_eq!(dispatch_ret, Value::Bool(true));
  Ok(())
}

#[test]
fn event_composed_path_includes_dom_ancestors() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__empty_len = new Event('z').composedPath().length;

globalThis.__cp_len = 0;
globalThis.__cp_first_is_target = false;
globalThis.__cp_last_is_window = false;
globalThis.__cp_contains_document = false;
globalThis.__cp_ids = "";
globalThis.__target_id = 0;

const target = document.createElement('div');
document.body.appendChild(target);
globalThis.__target_id = target.__fastrender_node_id;

target.addEventListener('x', (e) => {
  const path = e.composedPath();
  globalThis.__cp_len = path.length;
  globalThis.__cp_first_is_target = (path[0] === e.target);
  globalThis.__cp_last_is_window = (path[path.length - 1] === window);
  globalThis.__cp_contains_document = false;
  for (const item of path) {
    if (item === document) globalThis.__cp_contains_document = true;
  }
  globalThis.__cp_ids = path.map((item) => {
    if (item === window) return "window";
    if (item === document) return "document";
    if (item && typeof item === "object" && "__fastrender_node_id" in item) return String(item.__fastrender_node_id);
    return "null";
  }).join(",");
});

target.dispatchEvent(new Event('x', { bubbles: true }));
"#,
  )?;

  // `dispatchEvent` is synchronous, but run a checkpoint anyway to ensure any nested microtasks
  // don't affect assertions.
  event_loop.perform_microtask_checkpoint(&mut host)?;

  let (empty_len, target_id, cp_len, first_is_target, last_is_window, contains_document, cp_ids) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    (
      get_data_prop(&mut scope, global, "__empty_len"),
      get_data_prop(&mut scope, global, "__target_id"),
      get_data_prop(&mut scope, global, "__cp_len"),
      get_data_prop(&mut scope, global, "__cp_first_is_target"),
      get_data_prop(&mut scope, global, "__cp_last_is_window"),
      get_data_prop(&mut scope, global, "__cp_contains_document"),
      get_data_prop(&mut scope, global, "__cp_ids"),
    )
  };

  assert_eq!(empty_len, Value::Number(0.0));
  assert_eq!(first_is_target, Value::Bool(true));
  assert_eq!(last_is_window, Value::Bool(true));
  assert_eq!(contains_document, Value::Bool(true));

  let target_id = match target_id {
    Value::Number(n) => n as usize,
    other => {
      return Err(Error::Other(format!(
        "expected __target_id to be a number, got {other:?}"
      )))
    }
  };
  let cp_len = match cp_len {
    Value::Number(n) => n as usize,
    other => {
      return Err(Error::Other(format!(
        "expected __cp_len to be a number, got {other:?}"
      )))
    }
  };
  assert!(
    cp_len >= 4,
    "expected composedPath length >= 4, got {cp_len}"
  );

  let ids = {
    let window = host.window_mut();
    let (_vm, heap) = window.vm_and_heap_mut();
    get_string(heap, cp_ids)
  };
  assert!(
    ids.starts_with(&format!("{target_id},")),
    "expected __cp_ids to start with the target node id ({target_id}), got {ids:?}"
  );
  assert!(
    ids.ends_with("document,window"),
    "expected __cp_ids to end with document,window, got {ids:?}"
  );
  Ok(())
}

#[test]
fn event_composed_path_supports_opaque_event_targets() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__ok = false;

const parent = new EventTarget();
const child = new EventTarget(parent);

child.addEventListener('x', (e) => {
  const path = e.composedPath();
  globalThis.__ok = (
    path.length === 2 &&
    path[0] === child &&
    path[1] === parent &&
    !path.includes(window) &&
    !path.includes(document)
  );
});

child.dispatchEvent(new Event('x', { bubbles: true }));
"#,
  )?;

  event_loop.perform_microtask_checkpoint(&mut host)?;

  let ok = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    get_data_prop(&mut scope, global, "__ok")
  };
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn event_constructors_are_new_only() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host =
    WindowHost::new_with_fetcher(dom, "https://example.com/", Arc::new(InMemoryFetcher::default()))?;

  host.exec_script(
    r#"
globalThis.__event_call_err_name = "";
globalThis.__event_call_err_msg = "";
try {
  Event("x");
} catch (e) {
  globalThis.__event_call_err_name = e && e.name;
  globalThis.__event_call_err_msg = e && e.message;
}
globalThis.__event_new_type = new Event("x").type;

globalThis.__custom_event_call_err_name = "";
globalThis.__custom_event_call_err_msg = "";
try {
  CustomEvent("x");
} catch (e) {
  globalThis.__custom_event_call_err_name = e && e.name;
  globalThis.__custom_event_call_err_msg = e && e.message;
}
globalThis.__custom_event_new_detail = new CustomEvent("x", { detail: 123 }).detail;

globalThis.__pre_call_err_name = "";
globalThis.__pre_call_err_msg = "";
try {
  PromiseRejectionEvent("unhandledrejection", {
    promise: Promise.resolve("ok"),
    reason: "bad",
    cancelable: true,
  });
} catch (e) {
  globalThis.__pre_call_err_name = e && e.name;
  globalThis.__pre_call_err_msg = e && e.message;
}

const pr = new PromiseRejectionEvent("unhandledrejection", {
  promise: Promise.resolve("ok"),
  reason: "bad",
  cancelable: true,
});
globalThis.__pre_new_cancelable = pr.cancelable;
pr.preventDefault();
globalThis.__pre_new_default_prevented = pr.defaultPrevented;
globalThis.__pre_new_reason = pr.reason;
globalThis.__pre_new_promise_is_promise = pr.promise instanceof Promise;

try {
  pr.reason = "changed";
} catch (e) {}
globalThis.__pre_reason_after_write = pr.reason;
try {
  pr.promise = null;
} catch (e) {}
globalThis.__pre_promise_after_write_is_promise = pr.promise instanceof Promise;
"#,
  )?;

  let (
    event_call_name,
    event_call_msg,
    event_type,
    custom_call_name,
    custom_call_msg,
    custom_detail,
    pre_call_name,
    pre_call_msg,
    pre_cancelable,
    pre_default_prevented,
    pre_reason,
    pre_promise_is_promise,
    pre_reason_after_write,
    pre_promise_after_write_is_promise,
  ) = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    (
      get_data_prop(&mut scope, global, "__event_call_err_name"),
      get_data_prop(&mut scope, global, "__event_call_err_msg"),
      get_data_prop(&mut scope, global, "__event_new_type"),
      get_data_prop(&mut scope, global, "__custom_event_call_err_name"),
      get_data_prop(&mut scope, global, "__custom_event_call_err_msg"),
      get_data_prop(&mut scope, global, "__custom_event_new_detail"),
      get_data_prop(&mut scope, global, "__pre_call_err_name"),
      get_data_prop(&mut scope, global, "__pre_call_err_msg"),
      get_data_prop(&mut scope, global, "__pre_new_cancelable"),
      get_data_prop(&mut scope, global, "__pre_new_default_prevented"),
      get_data_prop(&mut scope, global, "__pre_new_reason"),
      get_data_prop(&mut scope, global, "__pre_new_promise_is_promise"),
      get_data_prop(&mut scope, global, "__pre_reason_after_write"),
      get_data_prop(&mut scope, global, "__pre_promise_after_write_is_promise"),
    )
  };

  let (
    event_call_name,
    event_call_msg,
    event_type,
    custom_call_name,
    custom_call_msg,
    pre_call_name,
    pre_call_msg,
    pre_reason,
    pre_reason_after_write,
  ) = {
    let window = host.host_mut().window_mut();
    let (_vm, heap) = window.vm_and_heap_mut();
    (
      get_string(heap, event_call_name),
      get_string(heap, event_call_msg),
      get_string(heap, event_type),
      get_string(heap, custom_call_name),
      get_string(heap, custom_call_msg),
      get_string(heap, pre_call_name),
      get_string(heap, pre_call_msg),
      get_string(heap, pre_reason),
      get_string(heap, pre_reason_after_write),
    )
  };

  assert_eq!(event_call_name, "TypeError");
  assert!(
    event_call_msg.contains("cannot be invoked without 'new'"),
    "unexpected Event() error message: {event_call_msg}"
  );
  assert_eq!(event_type, "x");

  assert_eq!(custom_call_name, "TypeError");
  assert!(
    custom_call_msg.contains("cannot be invoked without 'new'"),
    "unexpected CustomEvent() error message: {custom_call_msg}"
  );
  assert!(
    matches!(custom_detail, Value::Number(n) if n == 123.0),
    "expected new CustomEvent(...).detail to be 123"
  );

  assert_eq!(pre_call_name, "TypeError");
  assert!(
    pre_call_msg.contains("cannot be invoked without 'new'"),
    "unexpected PromiseRejectionEvent() error message: {pre_call_msg}"
  );
  assert!(
    matches!(pre_cancelable, Value::Bool(true)),
    "expected PromiseRejectionEvent.cancelable to be true"
  );
  assert!(
    matches!(pre_default_prevented, Value::Bool(true)),
    "expected PromiseRejectionEvent.preventDefault to set defaultPrevented"
  );
  assert_eq!(pre_reason, "bad");
  assert_eq!(
    pre_reason_after_write, "bad",
    "expected PromiseRejectionEvent.reason to be read-only"
  );
  assert!(
    matches!(pre_promise_is_promise, Value::Bool(true)),
    "expected PromiseRejectionEvent.promise to be a Promise"
  );
  assert!(
    matches!(pre_promise_after_write_is_promise, Value::Bool(true)),
    "expected PromiseRejectionEvent.promise to be read-only"
  );

  Ok(())
}

#[test]
fn promise_rejection_event_tasks_can_mutate_dom() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  assert!(host.dom().get_element_by_id("ur").is_none());
  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
window.addEventListener('unhandledrejection', (e) => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'ur';
  document.body.appendChild(d);
  e.preventDefault(); // suppress default host reporting
});

// Keep the rejected promise alive so the host can still dispatch the notification task.
globalThis.__ur = Promise.reject('boom');
"#,
  )?;

  // HTML dispatches unhandledrejection after a microtask checkpoint; drive one to enqueue the
  // notification task, then run tasks.
  event_loop.perform_microtask_checkpoint(&mut host)?;
  let mut errors: Vec<String> = Vec::new();
  assert_eq!(
    event_loop.run_until_idle_handling_errors(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
      |err| {
        errors.push(err.to_string());
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after dispatching the unhandledrejection task"
  );
  assert!(
    errors.is_empty(),
    "expected preventDefault() to suppress host error reporting, got errors={errors:?}"
  );

  assert!(
    host.dom().get_element_by_id("ur").is_some(),
    "expected unhandledrejection event listener to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn unhandledrejection_surfaces_host_error_by_default() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  // Trigger an unhandled rejection with no listeners to cancel default reporting.
  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
// Keep the rejected promise alive so the host can still dispatch the notification task.
globalThis.__ur_default = Promise.reject('boom');
"#,
  )?;

  event_loop.perform_microtask_checkpoint(&mut host)?;
  let mut errors: Vec<String> = Vec::new();
  assert_eq!(
    event_loop.run_until_idle_handling_errors(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
      |err| {
        errors.push(err.to_string());
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after dispatching the unhandledrejection task"
  );
  assert_eq!(
    errors.len(),
    1,
    "expected exactly one host error for an unhandled promise rejection, got errors={errors:?}"
  );
  assert!(
    errors[0].contains("Unhandled promise rejection"),
    "unexpected host error message: {:?}",
    errors[0]
  );
  assert!(
    errors[0].contains("boom"),
    "expected host error message to include rejection reason, got {:?}",
    errors[0]
  );
  Ok(())
}

#[test]
fn rejectionhandled_event_tasks_can_mutate_dom_and_receive_real_vm_host() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  assert!(host.dom().get_element_by_id("rh").is_none());
  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
window.addEventListener('unhandledrejection', (e) => e.preventDefault());
window.addEventListener('rejectionhandled', () => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'rh';
  document.body.appendChild(d);
});

// Trigger `unhandledrejection`, then attach a handler later to force `rejectionhandled`.
var p = Promise.reject('boom');
setTimeout(() => { p.catch(() => {}); }, 0);
"#,
  )?;

  // `unhandledrejection` is queued after a microtask checkpoint; drive one to enqueue the first
  // notification before running tasks (which will attach the handler and queue `rejectionhandled`).
  event_loop.perform_microtask_checkpoint(&mut host)?;
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 25,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after dispatching the rejectionhandled task"
  );
  assert!(
    host.dom().get_element_by_id("rh").is_some(),
    "expected rejectionhandled event listener to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn promise_rejection_event_task_roots_are_cleaned_up_when_queue_limits_reject_enqueue() -> Result<()>
{
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;

  // Allow promise rejection tracking, but ensure the event loop cannot enqueue an additional task
  // when the unhandledrejection notification is requested.
  let mut event_loop = EventLoop::<WindowHostState>::new();
  event_loop.set_queue_limits(QueueLimits {
    max_pending_tasks: 1,
    ..QueueLimits::default()
  });

  // Fill the task queue to the cap so the unhandledrejection task enqueue fails deterministically.
  event_loop.queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))?;

  host.exec_script_in_event_loop(
    &mut event_loop,
    // Keep the rejected promise alive so the host reaches the event-task enqueue path.
    "globalThis.__ur = Promise.reject('boom');",
  )?;

  let roots_before = host.window().heap().persistent_root_count();

  let err = event_loop
    .perform_microtask_checkpoint(&mut host)
    .expect_err("expected microtask checkpoint to fail when queue limits reject task enqueue");
  match &err {
    Error::Other(msg) => assert!(
      msg.contains("max pending tasks"),
      "expected enqueue failure to be due to max pending tasks, got: {msg}"
    ),
    other => panic!("expected Error::Other, got {other:?}"),
  }

  let roots_after = host.window().heap().persistent_root_count();
  assert_eq!(
    roots_before, roots_after,
    "promise rejection event-task enqueue failure leaked persistent roots"
  );

  Ok(())
}

#[test]
fn rejectionhandled_event_task_roots_are_cleaned_up_when_queue_limits_reject_enqueue() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;

  let mut event_loop = EventLoop::<WindowHostState>::new();
  event_loop.set_queue_limits(QueueLimits {
    max_pending_tasks: 1,
    ..QueueLimits::default()
  });

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
window.addEventListener('unhandledrejection', (e) => e.preventDefault());
// Keep the rejected promise alive so it remains eligible for rejectionhandled tracking.
globalThis.__ur2 = Promise.reject('boom');
"#,
  )?;

  // First checkpoint enqueues the unhandledrejection task (queue has capacity for it).
  event_loop.perform_microtask_checkpoint(&mut host)?;
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after dispatching unhandledrejection"
  );

  // Make the promise handled so the next checkpoint attempts to enqueue `rejectionhandled`.
  host.exec_script_in_event_loop(
    &mut event_loop,
    "globalThis.__ur2.catch(() => {});",
  )?;

  // Fill the task queue to the cap so the rejectionhandled task enqueue fails deterministically.
  event_loop.queue_task(TaskSource::Script, |_host, _event_loop| Ok(()))?;

  let roots_before = host.window().heap().persistent_root_count();
  let err = event_loop
    .perform_microtask_checkpoint(&mut host)
    .expect_err("expected microtask checkpoint to fail when queue limits reject task enqueue");
  match &err {
    Error::Other(msg) => assert!(
      msg.contains("max pending tasks"),
      "expected enqueue failure to be due to max pending tasks, got: {msg}"
    ),
    other => panic!("expected Error::Other, got {other:?}"),
  }
  let roots_after = host.window().heap().persistent_root_count();
  assert_eq!(
    roots_before, roots_after,
    "promise rejectionhandled event-task enqueue failure leaked persistent roots"
  );

  Ok(())
}

#[test]
fn readable_stream_pipe_through_internal_promises_do_not_trigger_unhandledrejection() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
 window.__unhandled = false;
 window.addEventListener('unhandledrejection', () => { window.__unhandled = true; });
 
 let ctrl;
 const rs = new ReadableStream({
   start(controller) { ctrl = controller; },
});

// Force the internal pipeThrough pump to throw inside its promise reaction callback by
// monkey-patching the writer returned by TextEncoderStream's writable.
const ts = new TextEncoderStream();
const origGetWriter = ts.writable.getWriter;
ts.writable.getWriter = function () {
  const writer = origGetWriter.call(this);
  writer.write = () => { throw new Error('boom'); };
  return writer;
};

rs.pipeThrough(ts);
ctrl.enqueue("x");
"#,
  )?;

  // Run the pump microtasks, then any resulting tasks (e.g. unhandledrejection).
  event_loop.perform_microtask_checkpoint(&mut host)?;
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 25,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle
  );

  let unhandled = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    get_data_prop(&mut scope, global, "__unhandled")
  };
  assert_eq!(unhandled, Value::Bool(false));
  Ok(())
}

#[test]
fn readable_stream_pipe_through_marks_internal_pipe_to_promise_handled() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__unhandled = false;
globalThis.__pipe_called = false;
globalThis.__same_return = false;

window.addEventListener('unhandledrejection', () => { globalThis.__unhandled = true; });

ReadableStream.prototype.pipeTo = function (_dest, _options) {
  globalThis.__pipe_called = true;
  // Keep the rejected promise alive so the host can still dispatch a notification task if the
  // Promise is not marked as handled.
  globalThis.__pipe_promise = Promise.reject('boom');
  return globalThis.__pipe_promise;
};

const rs = new ReadableStream();
const transform = { writable: {}, readable: {} };
const out = rs.pipeThrough(transform);
globalThis.__same_return = (out === transform.readable);
"#,
  )?;

  event_loop.perform_microtask_checkpoint(&mut host)?;
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle
  );

  let (pipe_called, same_return, unhandled) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let pipe_called = get_data_prop(&mut scope, global, "__pipe_called");
    let same_return = get_data_prop(&mut scope, global, "__same_return");
    let unhandled = get_data_prop(&mut scope, global, "__unhandled");
    (pipe_called, same_return, unhandled)
  };

  assert_eq!(pipe_called, Value::Bool(true));
  assert_eq!(same_return, Value::Bool(true));
  assert_eq!(unhandled, Value::Bool(false));
  Ok(())
}

#[test]
fn abort_signal_abort_event_handlers_can_mutate_dom() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  assert!(host.dom().get_element_by_id("abl").is_none());
  assert!(host.dom().get_element_by_id("abo").is_none());
  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__abort_is_event = false;
globalThis.__abort_type = "";
globalThis.__abort_cancelable = null;
const controller = new AbortController();
controller.signal.addEventListener('abort', (e) => {
  globalThis.__abort_is_event = e instanceof Event;
  globalThis.__abort_type = e.type;
  globalThis.__abort_cancelable = e.cancelable;
  const d = document.createElement('div');
  d.id = 'abl';
  document.body.appendChild(d);
});
controller.signal.onabort = () => {
  const d = document.createElement('div');
  d.id = 'abo';
  document.body.appendChild(d);
};
controller.abort();
"#,
  )?;

  // `abort()` dispatches synchronously, but run a checkpoint anyway to ensure any nested microtasks
  // don't affect assertions.
  event_loop.perform_microtask_checkpoint(&mut host)?;

  let (is_event, cancelable, type_) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let is_event = get_data_prop(&mut scope, global, "__abort_is_event");
    let cancelable = get_data_prop(&mut scope, global, "__abort_cancelable");
    let type_ = get_data_prop(&mut scope, global, "__abort_type");
    (is_event, cancelable, get_string(scope.heap(), type_))
  };
  assert_eq!(is_event, Value::Bool(true));
  assert_eq!(cancelable, Value::Bool(false));
  assert_eq!(type_, "abort");

  assert!(
    host.dom().get_element_by_id("abl").is_some(),
    "expected AbortSignal abort event listener to mutate the host DOM"
  );
  assert!(
    host.dom().get_element_by_id("abo").is_some(),
    "expected AbortSignal onabort handler to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn event_target_methods_reject_forged_dom_receivers() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host =
    WindowHost::new_with_fetcher(dom, "https://example.com/", Arc::new(InMemoryFetcher::default()))?;
  host.exec_script(
    r#"
globalThis.__err_name = "";
globalThis.__err_msg = "";
try {
  // This object would previously be treated as a DOM node wrapper because it has the same
  // `__fastrender_*` shape as real wrappers.
  const fake = {};
  Object.defineProperty(fake, "__fastrender_wrapper_document", { value: document });
  Object.defineProperty(fake, "__fastrender_node_id", { value: 0 });
  document.addEventListener.call(fake, "x", function () {});
  globalThis.__err_name = "no throw";
} catch (e) {
  globalThis.__err_name = e && e.name;
  globalThis.__err_msg = e && e.message;
}
"#,
  )?;

  let (name, msg) = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let name = get_data_prop(&mut scope, global, "__err_name");
    let msg = get_data_prop(&mut scope, global, "__err_msg");
    (
      get_string(scope.heap(), name),
      get_string(scope.heap(), msg),
    )
  };
  assert_eq!(name, "TypeError");
  assert_eq!(msg, "Illegal invocation");
  Ok(())
}

#[test]
fn request_animation_frame_callbacks_can_access_dom_shims() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script_in_event_loop(
      event_loop,
      r#"
requestAnimationFrame(() => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'raf';
  document.body.appendChild(d);
});
"#,
    )?;
    Ok(())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(event_loop.has_pending_animation_frame_callbacks());

  event_loop.run_animation_frame(&mut host)?;

  assert!(host.dom().get_element_by_id("raf").is_some());
  Ok(())
}

#[test]
fn abort_signal_abort_event_handlers_receive_real_vm_host() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_assert_non_dummy_vm_host(&mut host)?;

  assert!(host.dom().get_element_by_id("abl").is_none());
  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
const c = new AbortController();
c.signal.addEventListener('abort', () => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'abl';
  document.body.appendChild(d);
});
c.signal.onabort = () => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'ab';
  document.body.appendChild(d);
};
c.abort();
"#,
  )?;
  event_loop.perform_microtask_checkpoint(&mut host)?;
  assert!(host.dom().get_element_by_id("abl").is_some());
  assert!(host.dom().get_element_by_id("ab").is_some());
  Ok(())
}

#[test]
fn add_event_listener_signal_option_removes_listener_after_abort() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__count = 0;
const c = new AbortController();
document.body.addEventListener('x', () => { globalThis.__count++; }, { signal: c.signal });
document.body.dispatchEvent(new Event('x'));
c.abort();
document.body.dispatchEvent(new Event('x'));

// Already-aborted signals should prevent registration.
globalThis.__count2 = 0;
const c2 = new AbortController();
c2.abort();
document.body.addEventListener('y', () => { globalThis.__count2++; }, { signal: c2.signal });
document.body.dispatchEvent(new Event('y'));
"#,
  )?;

  event_loop.perform_microtask_checkpoint(&mut host)?;

  let (count, count2) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let count = get_data_prop(&mut scope, global, "__count");
    let count2 = get_data_prop(&mut scope, global, "__count2");
    (count, count2)
  };
  assert_eq!(count, Value::Number(1.0));
  assert_eq!(count2, Value::Number(0.0));
  Ok(())
}

#[test]
fn abort_signal_timeout_aborts_after_delay_and_sets_timeout_reason() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock.clone());
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__events = 0;
globalThis.__sig = AbortSignal.timeout(5);
__sig.addEventListener('abort', () => { globalThis.__events++; });
"#,
  )?;

  // Timer is not due yet.
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    assert_eq!(
      get_data_prop(&mut scope, global, "__events"),
      Value::Number(0.0)
    );
    let Value::Object(sig) = get_data_prop(&mut scope, global, "__sig") else {
      return Err(Error::Other(
        "expected AbortSignal.timeout to return an object".to_string(),
      ));
    };
    assert_eq!(
      get_data_prop(&mut scope, sig, "aborted"),
      Value::Bool(false)
    );
  }

  // Advance deterministic time so the timeout becomes due.
  clock.advance(std::time::Duration::from_millis(5));
  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let (events, aborted, reason_name) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let events = get_data_prop(&mut scope, global, "__events");
    let Value::Object(sig) = get_data_prop(&mut scope, global, "__sig") else {
      return Err(Error::Other("expected __sig to be an object".to_string()));
    };
    let aborted = get_data_prop(&mut scope, sig, "aborted");
    let Value::Object(reason) = get_data_prop(&mut scope, sig, "reason") else {
      return Err(Error::Other(
        "expected AbortSignal.reason to be an object".to_string(),
      ));
    };
    let reason_name_value = get_data_prop(&mut scope, reason, "name");
    let reason_name = get_string(scope.heap(), reason_name_value);
    (events, aborted, reason_name)
  };

  assert_eq!(events, Value::Number(1.0));
  assert_eq!(aborted, Value::Bool(true));
  assert_eq!(reason_name, "TimeoutError");
  Ok(())
}

#[test]
fn abort_signal_any_aborts_when_input_signal_aborts_and_forwards_reason() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__events = 0;
globalThis.__same_reason = false;

const c1 = new AbortController();
const c2 = new AbortController();

const s = AbortSignal.any([c1.signal, c2.signal]);
globalThis.__sig = s;
s.addEventListener('abort', () => { globalThis.__events++; });

c2.abort();
globalThis.__same_reason = (s.reason === c2.signal.reason);
"#,
  )?;

  // Abort dispatch is synchronous, but run a checkpoint to ensure any nested microtasks are drained.
  event_loop.perform_microtask_checkpoint(&mut host)?;

  let (events, aborted, same_reason, reason_name) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let events = get_data_prop(&mut scope, global, "__events");
    let same_reason = get_data_prop(&mut scope, global, "__same_reason");
    let Value::Object(sig) = get_data_prop(&mut scope, global, "__sig") else {
      return Err(Error::Other("expected __sig to be an object".to_string()));
    };
    let aborted = get_data_prop(&mut scope, sig, "aborted");
    let Value::Object(reason) = get_data_prop(&mut scope, sig, "reason") else {
      return Err(Error::Other(
        "expected AbortSignal.any composite reason to be an object".to_string(),
      ));
    };
    let reason_name_value = get_data_prop(&mut scope, reason, "name");
    let reason_name = get_string(scope.heap(), reason_name_value);
    (events, aborted, same_reason, reason_name)
  };

  assert_eq!(events, Value::Number(1.0));
  assert_eq!(aborted, Value::Bool(true));
  assert_eq!(same_reason, Value::Bool(true));
  assert_eq!(reason_name, "AbortError");
  Ok(())
}

#[test]
fn abort_signal_any_accepts_iterables() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__events = 0;
globalThis.__same_reason = false;

const c1 = new AbortController();
const c2 = new AbortController();

const iterable = {
  [Symbol.iterator]: function () {
    let i = 0;
    return {
      next: function () {
        i++;
        if (i === 1) return { value: c1.signal, done: false };
        if (i === 2) return { value: c2.signal, done: false };
        return { value: undefined, done: true };
      }
    };
  }
};

const s = AbortSignal.any(iterable);
globalThis.__sig = s;
s.addEventListener('abort', () => { globalThis.__events++; });

c2.abort();
globalThis.__same_reason = (s.reason === c2.signal.reason);
"#,
  )?;

  event_loop.perform_microtask_checkpoint(&mut host)?;

  let (events, aborted, same_reason, reason_name) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let events = get_data_prop(&mut scope, global, "__events");
    let same_reason = get_data_prop(&mut scope, global, "__same_reason");
    let Value::Object(sig) = get_data_prop(&mut scope, global, "__sig") else {
      return Err(Error::Other("expected __sig to be an object".to_string()));
    };
    let aborted = get_data_prop(&mut scope, sig, "aborted");
    let Value::Object(reason) = get_data_prop(&mut scope, sig, "reason") else {
      return Err(Error::Other(
        "expected AbortSignal.any composite reason to be an object".to_string(),
      ));
    };
    let reason_name_value = get_data_prop(&mut scope, reason, "name");
    let reason_name = get_string(scope.heap(), reason_name_value);
    (events, aborted, same_reason, reason_name)
  };

  assert_eq!(events, Value::Number(1.0));
  assert_eq!(aborted, Value::Bool(true));
  assert_eq!(same_reason, Value::Bool(true));
  assert_eq!(reason_name, "AbortError");
  Ok(())
}

#[test]
fn abort_signal_any_rejects_iterables_longer_than_limit() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__err_name = "";
globalThis.__err_message = "";
try {
  // Hostile input: an iterable can be unbounded or infinite. `AbortSignal.any` must cap how many
  // entries it will consume.
  //
  // Use a dense Array so iteration stays cheap even in debug builds: `Array.prototype[@@iterator]`
  // is implemented natively by the `vm-js` backend. (`Array.prototype.fill` is not available in
  // this minimal JS runtime, so build it manually.)
  const c = new AbortController();
  const signals = [];
  for (let i = 0; i < 1025; i++) signals.push(c.signal);
  AbortSignal.any(signals);
  globalThis.__err_name = "no throw";
} catch (e) {
  globalThis.__err_name = e && e.name;
  globalThis.__err_message = e && e.message;
}
"#,
  )?;

  let (name, message) = {
    let window = host.window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let err_value = get_data_prop(&mut scope, global, "__err_name");
    let msg_value = get_data_prop(&mut scope, global, "__err_message");
    (
      get_string(scope.heap(), err_value),
      get_string(scope.heap(), msg_value),
    )
  };
  assert_eq!(name, "TypeError");
  assert_eq!(message, "AbortSignal.any input is too large");
  Ok(())
}

#[test]
fn promise_jobs_abort_when_render_deadline_is_expired() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__ran = false;
Promise.resolve().then(() => { globalThis.__ran = true; });
"#,
  )?;

  // Install an already-expired render deadline so the VM callback budget has no time remaining.
  // Promise jobs are host-owned microtasks; they must not leak roots or run once the deadline is
  // exceeded.
  let deadline =
    render_control::RenderDeadline::new(Some(std::time::Duration::from_millis(0)), None);
  let _guard = render_control::DeadlineGuard::install(Some(&deadline));

  let _err = host
    .perform_microtask_checkpoint()
    .expect_err("expected microtask checkpoint to fail under expired deadline");

  let ran = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    get_data_prop(&mut scope, global, "__ran")
  };
  assert_eq!(ran, Value::Bool(false));
  Ok(())
}

#[test]
fn promise_any_resolves_first_fulfilled_value() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__result = "";
Promise.any(["a", "b"]).then(
  function (v) { globalThis.__result = v; },
  function () { globalThis.__result = "rejected"; }
);
"#,
  )?;

  let before = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__result");
    get_string(scope.heap(), value)
  };
  assert_eq!(before, "");

  host.perform_microtask_checkpoint()?;

  let after = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__result");
    get_string(scope.heap(), value)
  };
  assert_eq!(after, "a");
  Ok(())
}

#[test]
fn promise_any_rejects_with_aggregate_error_when_all_reject() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__err_name = "";
globalThis.__err0 = "";
Promise.any([Promise.reject("x"), Promise.reject("y")]).then(
  function () { globalThis.__err_name = "resolved"; },
  function (e) {
    globalThis.__err_name = e && e.name;
    globalThis.__err0 = e && e.errors && e.errors[0];
  }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let (name, err0) = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let name = get_data_prop(&mut scope, global, "__err_name");
    let err0 = get_data_prop(&mut scope, global, "__err0");
    (
      get_string(scope.heap(), name),
      get_string(scope.heap(), err0),
    )
  };

  assert_eq!(name, "AggregateError");
  assert_eq!(err0, "x");
  Ok(())
}

#[test]
fn promise_all_settled_reports_fulfilled_and_rejected_entries() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__status0 = "";
globalThis.__value0 = "";
globalThis.__status1 = "";
globalThis.__reason1 = "";
Promise.allSettled([Promise.resolve("a"), Promise.reject("b")]).then(function (res) {
  globalThis.__status0 = res[0].status;
  globalThis.__value0 = res[0].value;
  globalThis.__status1 = res[1].status;
  globalThis.__reason1 = res[1].reason;
});
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let (status0, value0, status1, reason1) = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let status0 = get_data_prop(&mut scope, global, "__status0");
    let value0 = get_data_prop(&mut scope, global, "__value0");
    let status1 = get_data_prop(&mut scope, global, "__status1");
    let reason1 = get_data_prop(&mut scope, global, "__reason1");
    (
      get_string(scope.heap(), status0),
      get_string(scope.heap(), value0),
      get_string(scope.heap(), status1),
      get_string(scope.heap(), reason1),
    )
  };

  assert_eq!(status0, "fulfilled");
  assert_eq!(value0, "a");
  assert_eq!(status1, "rejected");
  assert_eq!(reason1, "b");
  Ok(())
}

#[test]
fn promise_all_resolves_values_in_input_order() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "";
Promise.all([Promise.resolve("a"), "b"]).then(
  function (res) { globalThis.__out = res[0] + "," + res[1]; },
  function (e) { globalThis.__out = "rejected:" + e; }
);
"#,
  )?;

  let before = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(before, "");

  host.perform_microtask_checkpoint()?;

  let after = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(after, "a,b");
  Ok(())
}

#[test]
fn promise_all_rejects_with_first_rejection_reason() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "";
Promise.all([Promise.reject("x"), Promise.resolve("y")]).then(
  function () { globalThis.__out = "resolved"; },
  function (e) { globalThis.__out = "rejected:" + e; }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let out = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(out, "rejected:x");
  Ok(())
}

#[test]
fn promise_race_resolves_first_settled_value() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "";
Promise.race([Promise.resolve("a"), Promise.resolve("b")]).then(
  function (v) { globalThis.__out = "resolved:" + v; },
  function (e) { globalThis.__out = "rejected:" + e; }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let out = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(out, "resolved:a");
  Ok(())
}

#[test]
fn promise_race_rejects_first_rejection_reason() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "";
Promise.race([Promise.reject("x"), Promise.resolve("y")]).then(
  function (v) { globalThis.__out = "resolved:" + v; },
  function (e) { globalThis.__out = "rejected:" + e; }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let out = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(out, "rejected:x");
  Ok(())
}

#[test]
fn promise_all_resolves_empty_iterable() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__ok = false;
Promise.all([]).then(function (res) { globalThis.__ok = res.length === 0; });
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let ok = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    get_data_prop(&mut scope, global, "__ok")
  };
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_all_rejects_non_iterable_argument() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "";
Promise.all(1).then(
  function () { globalThis.__out = "resolved"; },
  function (e) { globalThis.__out = e && e.name; }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let out = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(out, "TypeError");
  Ok(())
}

#[test]
fn promise_all_settled_resolves_empty_iterable() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__ok = false;
Promise.allSettled([]).then(function (res) { globalThis.__ok = res.length === 0; });
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let ok = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    get_data_prop(&mut scope, global, "__ok")
  };
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_any_rejects_empty_iterable_with_aggregate_error() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__name = "";
globalThis.__errors_len = "";
Promise.any([]).then(
  function () { globalThis.__name = "resolved"; },
  function (e) {
    globalThis.__name = e && e.name;
    globalThis.__errors_len = (e && e.errors) ? "" + e.errors.length : "missing";
  }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let (name, errors_len) = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let name = get_data_prop(&mut scope, global, "__name");
    let errors_len = get_data_prop(&mut scope, global, "__errors_len");
    (
      get_string(scope.heap(), name),
      get_string(scope.heap(), errors_len),
    )
  };
  assert_eq!(name, "AggregateError");
  assert_eq!(errors_len, "0");
  Ok(())
}

#[test]
fn promise_race_empty_iterable_remains_pending() -> Result<()> {
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "https://example.com/")?;
  host.exec_script(
    r#"
globalThis.__out = "init";
Promise.race([]).then(
  function () { globalThis.__out = "resolved"; },
  function () { globalThis.__out = "rejected"; }
);
"#,
  )?;

  host.perform_microtask_checkpoint()?;

  let out = {
    let window = host.host_mut().window_mut();
    let global = window.global_object();
    let (_vm, heap) = window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let value = get_data_prop(&mut scope, global, "__out");
    get_string(scope.heap(), value)
  };
  assert_eq!(out, "init");
  Ok(())
}

#[test]
fn location_url_components_are_exposed_to_js_execution() -> Result<()> {
  let url = "https://example.com:8080/path/to/page?query=1#hash";
  let mut realm =
    WindowRealm::new_with_js_execution_options(WindowRealmConfig::new(url), js_opts_for_test())
      .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script(
      "location.protocol + '|' + location.host + '|' + location.hostname + '|' + location.port + '|' + location.pathname + '|' + location.search + '|' + location.hash + '|' + location.origin",
    )
    .map_err(|e| Error::Other(e.to_string()))?;
  assert_eq!(
    get_string(realm.heap(), value),
    "https:|example.com:8080|example.com|8080|/path/to/page|?query=1|#hash|https://example.com:8080"
  );
  Ok(())
}

#[test]
fn document_head_and_body_reflect_dom_ids() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head id=h></head><body id=b></body></html>",
  )?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
  globalThis.__head_id = document.head.id;
  globalThis.__body_id = document.body.id;
  globalThis.__head_same = document.head === document.head;
  globalThis.__body_same = document.body === document.body;
  document.body.id = "new";
  globalThis.__body_id_after = document.body.id;
  "#,
  )?;

  let (head_id, body_id, body_id_after, head_same, body_same) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    let head_value = get_data_prop(&mut scope, global, "__head_id");
    let body_value = get_data_prop(&mut scope, global, "__body_id");
    let body_after_value = get_data_prop(&mut scope, global, "__body_id_after");
    let head_same = get_data_prop(&mut scope, global, "__head_same");
    let body_same = get_data_prop(&mut scope, global, "__body_same");
    (
      get_string(scope.heap(), head_value),
      get_string(scope.heap(), body_value),
      get_string(scope.heap(), body_after_value),
      head_same,
      body_same,
    )
  };

  assert_eq!(head_id, "h");
  assert_eq!(body_id, "b");
  assert_eq!(body_id_after, "new");
  assert_eq!(head_same, Value::Bool(true));
  assert_eq!(body_same, Value::Bool(true));

  let body_node = host
    .dom()
    .body()
    .expect("expected body element to exist for HTML document");
  assert_eq!(host.dom().element_id(body_node), "new");

  Ok(())
}

#[test]
fn document_get_element_by_id_returns_stable_wrapper() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head></head><body><div id=x></div></body></html>",
  )?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
  globalThis.__missing = document.getElementById("missing") === null;
  globalThis.__empty = document.getElementById("") === null;
  const el = document.getElementById("x");
  globalThis.__same = el === document.getElementById("x");
  globalThis.__id_before = el.id;
  el.id = "y";
  globalThis.__old_missing = document.getElementById("x") === null;
  const el2 = document.getElementById("y");
  globalThis.__same_after = el === el2;
  globalThis.__id_after = el2.id;
  "#,
  )?;

  let (missing, empty, same, old_missing, same_after, id_before, id_after) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    let missing = get_data_prop(&mut scope, global, "__missing");
    let empty = get_data_prop(&mut scope, global, "__empty");
    let same = get_data_prop(&mut scope, global, "__same");
    let old_missing = get_data_prop(&mut scope, global, "__old_missing");
    let same_after = get_data_prop(&mut scope, global, "__same_after");
    let id_before_v = get_data_prop(&mut scope, global, "__id_before");
    let id_after_v = get_data_prop(&mut scope, global, "__id_after");
    (
      missing,
      empty,
      same,
      old_missing,
      same_after,
      get_string(scope.heap(), id_before_v),
      get_string(scope.heap(), id_after_v),
    )
  };

  assert_eq!(missing, Value::Bool(true));
  assert_eq!(empty, Value::Bool(true));
  assert_eq!(same, Value::Bool(true));
  assert_eq!(old_missing, Value::Bool(true));
  assert_eq!(same_after, Value::Bool(true));
  assert_eq!(id_before, "x");
  assert_eq!(id_after, "y");

  assert!(host.dom().get_element_by_id("x").is_none());
  let node = host
    .dom()
    .get_element_by_id("y")
    .expect("expected DOM to reflect updated id");
  assert_eq!(host.dom().element_id(node), "y");

  Ok(())
}

#[test]
fn document_query_selector_returns_stable_wrapper() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head></head><body><div class=x id=a></div></body></html>",
  )?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r###"
  const el = document.querySelector(".x");
  globalThis.__qs_found = (el !== null);
  globalThis.__qs_same = (el === document.querySelector(".x"));
  globalThis.__qs_id = el && el.getAttribute("id");
  try {
    document.querySelector("##");
    globalThis.__qs_bad = "no";
  } catch (e) {
    globalThis.__qs_bad = e.name;
  }
  "###,
  )?;

  let (qs_found, qs_same, qs_id, qs_bad) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    let found = get_data_prop(&mut scope, global, "__qs_found");
    let same = get_data_prop(&mut scope, global, "__qs_same");
    let id = get_data_prop(&mut scope, global, "__qs_id");
    let bad = get_data_prop(&mut scope, global, "__qs_bad");
    (
      found,
      same,
      get_string(scope.heap(), id),
      get_string(scope.heap(), bad),
    )
  };

  assert_eq!(qs_found, Value::Bool(true));
  assert_eq!(qs_same, Value::Bool(true));
  assert_eq!(qs_id, "a");
  assert_eq!(qs_bad, "SyntaxError");

  Ok(())
}

#[test]
fn element_query_selector_all_and_matches_closest_work() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><head></head><body>\
     <div id=a class=wrap><span id=a_inner class='inner other'></span></div>\
     <div id=b class=wrap><span id=b_inner class=inner></span></div>\
     </body></html>",
  )?;
  // This test exercises multiple selector queries (including `:scope` and invalid selectors).
  // The default per-spin JS wall-time budget is intentionally conservative for hostile scripts,
  // so relax it here to focus on correctness of selector APIs.
  let mut opts = js_opts_for_test();
  opts.event_loop_run_limits.max_wall_time = Some(std::time::Duration::from_secs(2));
  let mut host = WindowHostState::new_with_fetcher_and_options(
    Dom2Document::from_renderer_dom(&renderer_dom),
    "https://example.com/",
    Arc::new(InMemoryFetcher::default()),
    opts,
  )?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r###"
  const a = document.getElementById("a");
  const inner = a.querySelector(".inner");
  globalThis.__el_qs_id = inner && inner.id;

  const doc_all = document.querySelectorAll(".inner");
  globalThis.__doc_all_len = doc_all.length;
  globalThis.__doc_all_0 = doc_all[0] && doc_all[0].id;
  globalThis.__doc_all_1 = doc_all[1] && doc_all[1].id;

  const a_all = a.querySelectorAll(".inner");
  globalThis.__a_all_len = a_all.length;
  globalThis.__a_all_0 = a_all[0] && a_all[0].id;

  globalThis.__scope_same = (a.querySelector(":scope") === a);
  globalThis.__a_matches = a.matches("div.wrap");
  globalThis.__inner_matches = inner.matches("div span.inner");
  globalThis.__closest_ok = (inner.closest("#a") === a);

  try {
    a.querySelectorAll("##");
    globalThis.__bad_qsa = "no";
  } catch (e) {
    globalThis.__bad_qsa = e.name;
  }
  try {
    inner.matches("##");
    globalThis.__bad_matches = "no";
  } catch (e) {
    globalThis.__bad_matches = e.name;
  }
  try {
    inner.closest("##");
    globalThis.__bad_closest = "no";
  } catch (e) {
    globalThis.__bad_closest = e.name;
  }
  "###,
  )?;

  let (
    el_qs_id,
    doc_all_len,
    doc_all_0,
    doc_all_1,
    a_all_len,
    a_all_0,
    scope_same,
    a_matches,
    inner_matches,
    closest_ok,
    bad_qsa,
    bad_matches,
    bad_closest,
  ) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    let el_qs_id_v = get_data_prop(&mut scope, global, "__el_qs_id");
    let doc_all_len = get_data_prop(&mut scope, global, "__doc_all_len");
    let doc_all_0_v = get_data_prop(&mut scope, global, "__doc_all_0");
    let doc_all_1_v = get_data_prop(&mut scope, global, "__doc_all_1");
    let a_all_len = get_data_prop(&mut scope, global, "__a_all_len");
    let a_all_0_v = get_data_prop(&mut scope, global, "__a_all_0");
    let scope_same = get_data_prop(&mut scope, global, "__scope_same");
    let a_matches = get_data_prop(&mut scope, global, "__a_matches");
    let inner_matches = get_data_prop(&mut scope, global, "__inner_matches");
    let closest_ok = get_data_prop(&mut scope, global, "__closest_ok");
    let bad_qsa_v = get_data_prop(&mut scope, global, "__bad_qsa");
    let bad_matches_v = get_data_prop(&mut scope, global, "__bad_matches");
    let bad_closest_v = get_data_prop(&mut scope, global, "__bad_closest");

    let heap = scope.heap();
    (
      get_string(heap, el_qs_id_v),
      doc_all_len,
      get_string(heap, doc_all_0_v),
      get_string(heap, doc_all_1_v),
      a_all_len,
      get_string(heap, a_all_0_v),
      scope_same,
      a_matches,
      inner_matches,
      closest_ok,
      get_string(heap, bad_qsa_v),
      get_string(heap, bad_matches_v),
      get_string(heap, bad_closest_v),
    )
  };

  assert_eq!(el_qs_id, "a_inner");
  assert_eq!(doc_all_len, Value::Number(2.0));
  assert_eq!(doc_all_0, "a_inner");
  assert_eq!(doc_all_1, "b_inner");
  assert_eq!(a_all_len, Value::Number(1.0));
  assert_eq!(a_all_0, "a_inner");
  assert_eq!(scope_same, Value::Bool(true));
  assert_eq!(a_matches, Value::Bool(true));
  assert_eq!(inner_matches, Value::Bool(true));
  assert_eq!(closest_ok, Value::Bool(true));
  assert_eq!(bad_qsa, "SyntaxError");
  assert_eq!(bad_matches, "SyntaxError");
  assert_eq!(bad_closest, "SyntaxError");

  Ok(())
}

#[test]
fn document_create_element_and_append_child_update_dom() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
  const el = document.createElement("div");
  el.setAttribute("id", "x");
  el.setAttribute("data-test", "1");
  globalThis.__data_test = el.getAttribute("data-test");
  globalThis.__missing_attr = (el.getAttribute("missing") === null);
  const ret = document.body.appendChild(el);
  globalThis.__append_same = (ret === el);
  globalThis.__found_same = (document.getElementById("x") === el);
  "#,
  )?;

  let (append_same, found_same, data_test, missing_attr) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    (
      get_data_prop(&mut scope, global, "__append_same"),
      get_data_prop(&mut scope, global, "__found_same"),
      get_data_prop(&mut scope, global, "__data_test"),
      get_data_prop(&mut scope, global, "__missing_attr"),
    )
  };

  assert_eq!(append_same, Value::Bool(true));
  assert_eq!(found_same, Value::Bool(true));
  assert_eq!(get_string(host.window().heap(), data_test), "1");
  assert_eq!(missing_attr, Value::Bool(true));

  let node = host
    .dom()
    .get_element_by_id("x")
    .expect("expected appended element to be reachable via get_element_by_id");
  let body = host
    .dom()
    .body()
    .expect("expected HTML document to have a body element");
  assert_eq!(
    host
      .dom()
      .parent(node)
      .expect("expected dom2::Document::parent to succeed"),
    Some(body)
  );
  assert_eq!(
    host
      .dom()
      .get_attribute(node, "data-test")
      .expect("expected get_attribute to succeed"),
    Some("1")
  );

  Ok(())
}

#[test]
fn webidl_shadow_root_is_fragment_like_and_not_detachable_open() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = WebIdlWindowHostStateForTest::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  exec_script_in_webidl_window_host(
    &mut host,
    r#"
  const hostEl = document.createElement("div");
  document.body.appendChild(hostEl);

  const sr = hostEl.attachShadow({ mode: "open" });
  const child = document.createElement("b");
  sr.appendChild(child);

  globalThis.__remove_err = "no error";
  try { hostEl.removeChild(sr); } catch (e) { globalThis.__remove_err = e && e.name; }

  globalThis.__insert_before_err = "no error";
  try { hostEl.insertBefore(document.createElement("i"), sr); } catch (e) { globalThis.__insert_before_err = e && e.name; }

  const ret = document.body.appendChild(sr);
  globalThis.__append_ret_same = (ret === sr);
  globalThis.__sr_len = sr.childNodes.length;
  globalThis.__child_parent_is_body = (child.parentNode === document.body);
  globalThis.__shadow_root_still_attached = (hostEl.shadowRoot === sr);
  "#,
  )?;

  let (
    remove_err,
    insert_before_err,
    append_ret_same,
    sr_len,
    child_parent_is_body,
    shadow_root_still_attached,
  ) = {
    let global = host.window.global_object();
    let (_vm, heap) = host.window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let remove_err_v = get_data_prop(&mut scope, global, "__remove_err");
    let insert_before_err_v = get_data_prop(&mut scope, global, "__insert_before_err");
    (
      get_string(scope.heap(), remove_err_v),
      get_string(scope.heap(), insert_before_err_v),
      get_data_prop(&mut scope, global, "__append_ret_same"),
      get_data_prop(&mut scope, global, "__sr_len"),
      get_data_prop(&mut scope, global, "__child_parent_is_body"),
      get_data_prop(&mut scope, global, "__shadow_root_still_attached"),
    )
  };

  assert_eq!(remove_err, "NotFoundError");
  assert_eq!(insert_before_err, "NotFoundError");
  assert_eq!(append_ret_same, Value::Bool(true));
  assert_eq!(sr_len, Value::Number(0.0));
  assert_eq!(child_parent_is_body, Value::Bool(true));
  assert_eq!(shadow_root_still_attached, Value::Bool(true));
  Ok(())
}

#[test]
fn webidl_shadow_root_is_fragment_like_and_not_detachable_closed() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = WebIdlWindowHostStateForTest::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  exec_script_in_webidl_window_host(
    &mut host,
    r#"
  const hostEl = document.createElement("div");
  document.body.appendChild(hostEl);

  const sr = hostEl.attachShadow({ mode: "closed" });
  const child = document.createElement("b");
  sr.appendChild(child);

  globalThis.__remove_err = "no error";
  try { hostEl.removeChild(sr); } catch (e) { globalThis.__remove_err = e && e.name; }

  globalThis.__insert_before_err = "no error";
  try { hostEl.insertBefore(document.createElement("i"), sr); } catch (e) { globalThis.__insert_before_err = e && e.name; }

  const ret = document.body.appendChild(sr);
  globalThis.__append_ret_same = (ret === sr);
  globalThis.__sr_len = sr.childNodes.length;
  globalThis.__child_parent_is_body = (child.parentNode === document.body);
  "#,
  )?;

  let (remove_err, insert_before_err, append_ret_same, sr_len, child_parent_is_body) = {
    let global = host.window.global_object();
    let (_vm, heap) = host.window.vm_and_heap_mut();
    let mut scope = heap.scope();
    let remove_err_v = get_data_prop(&mut scope, global, "__remove_err");
    let insert_before_err_v = get_data_prop(&mut scope, global, "__insert_before_err");
    (
      get_string(scope.heap(), remove_err_v),
      get_string(scope.heap(), insert_before_err_v),
      get_data_prop(&mut scope, global, "__append_ret_same"),
      get_data_prop(&mut scope, global, "__sr_len"),
      get_data_prop(&mut scope, global, "__child_parent_is_body"),
    )
  };

  assert_eq!(remove_err, "NotFoundError");
  assert_eq!(insert_before_err, "NotFoundError");
  assert_eq!(append_ret_same, Value::Bool(true));
  assert_eq!(sr_len, Value::Number(0.0));
  assert_eq!(child_parent_is_body, Value::Bool(true));
  Ok(())
}

#[test]
fn webidl_range_basic_and_query_apis() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(concat!(
    "<!doctype html>",
    "<html><head></head><body>",
    "<div id=\"div\"><span id=\"s0\">s0</span><span id=\"s1\">s1</span><span id=\"s2\">s2</span></div>",
    "</body></html>",
  ))?;
  let mut host =
    WebIdlWindowHostStateForTest::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  exec_script_in_webidl_window_host(
    &mut host,
    r#"
  // Range constructor + initial state.
  const r1 = document.createRange();
  globalThis.__doc_create_is_range = (r1 instanceof Range);
  globalThis.__doc_create_start_is_doc = (r1.startContainer === document);
  globalThis.__doc_create_end_is_doc = (r1.endContainer === document);
  globalThis.__doc_create_collapsed = r1.collapsed;

  const r2 = new Range();
  globalThis.__ctor_is_range = (r2 instanceof Range);
  globalThis.__ctor_start_is_doc = (r2.startContainer === document);
  globalThis.__ctor_end_is_doc = (r2.endContainer === document);
  globalThis.__ctor_collapsed = r2.collapsed;

  // Query APIs (comparePoint/isPointInRange/intersectsNode).
  const range = new Range();
  const div = document.getElementById('div');
  const s0 = document.getElementById('s0');
  const s1 = document.getElementById('s1');
  const s2 = document.getElementById('s2');

  range.setStart(div, 1);
  range.setEnd(div, 2);
  globalThis.__compare_before = range.comparePoint(div, 0);
  globalThis.__compare_inside = range.comparePoint(div, 1);
  globalThis.__compare_after = range.comparePoint(div, 3);

  globalThis.__point_before = range.isPointInRange(div, 0);
  globalThis.__point_inside = range.isPointInRange(div, 1);
  globalThis.__point_end = range.isPointInRange(div, 2);
  globalThis.__point_after = range.isPointInRange(div, 3);

  // Range encloses s0.
  range.setStart(div, 0);
  range.setEnd(div, 1);
  globalThis.__intersects_s0 = range.intersectsNode(s0);
  globalThis.__intersects_s1 = range.intersectsNode(s1);
  globalThis.__intersects_s2 = range.intersectsNode(s2);
  "#,
  )?;

  let (
    doc_create_is_range,
    doc_create_start_is_doc,
    doc_create_end_is_doc,
    doc_create_collapsed,
    ctor_is_range,
    ctor_start_is_doc,
    ctor_end_is_doc,
    ctor_collapsed,
    compare_before,
    compare_inside,
    compare_after,
    point_before,
    point_inside,
    point_end,
    point_after,
    intersects_s0,
    intersects_s1,
    intersects_s2,
  ) = {
    let global = host.window.global_object();
    let (_vm, heap) = host.window.vm_and_heap_mut();
    let mut scope = heap.scope();
    (
      get_data_prop(&mut scope, global, "__doc_create_is_range"),
      get_data_prop(&mut scope, global, "__doc_create_start_is_doc"),
      get_data_prop(&mut scope, global, "__doc_create_end_is_doc"),
      get_data_prop(&mut scope, global, "__doc_create_collapsed"),
      get_data_prop(&mut scope, global, "__ctor_is_range"),
      get_data_prop(&mut scope, global, "__ctor_start_is_doc"),
      get_data_prop(&mut scope, global, "__ctor_end_is_doc"),
      get_data_prop(&mut scope, global, "__ctor_collapsed"),
      get_data_prop(&mut scope, global, "__compare_before"),
      get_data_prop(&mut scope, global, "__compare_inside"),
      get_data_prop(&mut scope, global, "__compare_after"),
      get_data_prop(&mut scope, global, "__point_before"),
      get_data_prop(&mut scope, global, "__point_inside"),
      get_data_prop(&mut scope, global, "__point_end"),
      get_data_prop(&mut scope, global, "__point_after"),
      get_data_prop(&mut scope, global, "__intersects_s0"),
      get_data_prop(&mut scope, global, "__intersects_s1"),
      get_data_prop(&mut scope, global, "__intersects_s2"),
    )
  };

  assert_eq!(doc_create_is_range, Value::Bool(true));
  assert_eq!(doc_create_start_is_doc, Value::Bool(true));
  assert_eq!(doc_create_end_is_doc, Value::Bool(true));
  assert_eq!(doc_create_collapsed, Value::Bool(true));
  assert_eq!(ctor_is_range, Value::Bool(true));
  assert_eq!(ctor_start_is_doc, Value::Bool(true));
  assert_eq!(ctor_end_is_doc, Value::Bool(true));
  assert_eq!(ctor_collapsed, Value::Bool(true));

  assert_eq!(compare_before, Value::Number(-1.0));
  assert_eq!(compare_inside, Value::Number(0.0));
  assert_eq!(compare_after, Value::Number(1.0));

  assert_eq!(point_before, Value::Bool(false));
  assert_eq!(point_inside, Value::Bool(true));
  assert_eq!(point_end, Value::Bool(true));
  assert_eq!(point_after, Value::Bool(false));

  assert_eq!(intersects_s0, Value::Bool(true));
  assert_eq!(intersects_s1, Value::Bool(false));
  assert_eq!(intersects_s2, Value::Bool(false));
  Ok(())
}

#[test]
fn document_current_script_is_visible_to_js_execution() -> Result<()> {
  #[derive(Default)]
  struct NoopHostHooks;

  impl VmHostHooks for NoopHostHooks {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
  }

  #[derive(Default)]
  struct JsExecutor {
    observed: Vec<usize>,
    wrapper_identity_ok: Vec<bool>,
  }

impl ScriptBlockExecutor<WindowHostState> for JsExecutor {
    fn execute_script(
      &mut self,
      host: &mut WindowHostState,
      _orchestrator: &mut ScriptOrchestrator,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      let stable = {
        let (vm_host, realm) = host.vm_host_and_window_realm()?;
        let mut hooks = NoopHostHooks::default();
        realm
          .exec_script_with_host_and_hooks(
            vm_host,
            &mut hooks,
            "document.currentScript === document.currentScript",
          )
          .map_err(|e| Error::Other(e.to_string()))?
      };
      let Value::Bool(stable) = stable else {
        return Err(Error::Other(
          "expected document.currentScript identity check to return a bool".to_string(),
        ));
      };
      self.wrapper_identity_ok.push(stable);

      let node_id = {
        let (vm_host, realm) = host.vm_host_and_window_realm()?;
        let mut hooks = NoopHostHooks::default();
        realm
          .exec_script_with_host_and_hooks(
            vm_host,
            &mut hooks,
            "document.currentScript.__fastrender_node_id",
          )
          .map_err(|e| Error::Other(e.to_string()))?
      };
      let Value::Number(n) = node_id else {
        return Err(Error::Other(
          "expected document.currentScript.__fastrender_node_id to be a number".to_string(),
        ));
      };
      let as_usize = n as usize;
      if (as_usize as f64) != n {
        return Err(Error::Other(format!(
          "expected document.currentScript.__fastrender_node_id to be an integer, got {n:?}"
        )));
      }
      self.observed.push(as_usize);
      Ok(())
    }
  }

  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><script></script><script></script>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let scripts = find_script_elements(host.dom());
  assert_eq!(scripts.len(), 2);

  // Outside execution, currentScript is null.
  {
    let realm = host.window_mut();
    let value = realm
      .exec_script("document.currentScript")
      .map_err(|e| Error::Other(e.to_string()))?;
    assert_eq!(value, Value::Null);
  }

  let mut orchestrator = ScriptOrchestrator::new();
  let mut executor = JsExecutor::default();

  orchestrator.execute_script_element(&mut host, scripts[0], ScriptType::Classic, &mut executor)?;
  orchestrator.execute_script_element(&mut host, scripts[1], ScriptType::Classic, &mut executor)?;

  assert_eq!(executor.wrapper_identity_ok, vec![true, true]);
  assert_eq!(
    executor.observed,
    vec![scripts[0].index(), scripts[1].index()]
  );
  Ok(())
}

#[derive(Debug, Clone)]
struct StubResponse {
  bytes: Vec<u8>,
  status: u16,
}

#[derive(Debug)]
struct InMemoryFetcher {
  routes: HashMap<String, StubResponse>,
  request_urls: Mutex<Vec<String>>,
  last_request_headers: Mutex<Vec<(String, String)>>,
  last_request_body: Mutex<Option<Vec<u8>>>,
  last_request_credentials_mode: Mutex<Option<FetchCredentialsMode>>,
}

impl InMemoryFetcher {
  fn new() -> Self {
    Self {
      routes: HashMap::new(),
      request_urls: Mutex::new(Vec::new()),
      last_request_headers: Mutex::new(Vec::new()),
      last_request_body: Mutex::new(None),
      last_request_credentials_mode: Mutex::new(None),
    }
  }

  fn with_response(mut self, url: &str, bytes: impl Into<Vec<u8>>, status: u16) -> Self {
    self.routes.insert(
      url.to_string(),
      StubResponse {
        bytes: bytes.into(),
        status,
      },
    );
    self
  }

  fn lookup(&self, url: &str) -> Result<StubResponse> {
    self
      .routes
      .get(url)
      .cloned()
      .ok_or_else(|| Error::Other(format!("no stubbed response for {url}")))
  }

  fn last_request_headers(&self) -> Vec<(String, String)> {
    self
      .last_request_headers
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  fn request_urls(&self) -> Vec<String> {
    self
      .request_urls
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  fn last_request_body(&self) -> Option<Vec<u8>> {
    self
      .last_request_body
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  fn last_request_credentials_mode(&self) -> Option<FetchCredentialsMode> {
    *self
      .last_request_credentials_mode
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }
}

impl Default for InMemoryFetcher {
  fn default() -> Self {
    Self::new()
  }
}

impl ResourceFetcher for InMemoryFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let fetch = FetchRequest::new(url, FetchDestination::Fetch);
    self.fetch_http_request(HttpRequest::new(fetch, "GET"))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self.fetch_http_request(HttpRequest::new(req, "GET"))
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    {
      let mut lock = self
        .request_urls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      lock.push(req.fetch.url.to_string());
    }
    {
      let mut lock = self
        .last_request_headers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      *lock = req.headers.to_vec();
    }
    {
      let mut lock = self
        .last_request_body
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      *lock = req.body.map(|body| body.to_vec());
    }
    {
      let mut lock = self
        .last_request_credentials_mode
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      *lock = Some(req.fetch.credentials_mode);
    }

    let stub = self.lookup(req.fetch.url)?;
    let mut resource = FetchedResource::new(stub.bytes, None);
    resource.status = Some(stub.status);
    // Echo request headers back as response headers so JS can observe them via `Response.headers`
    // if desired.
    resource.response_headers = Some(req.headers.to_vec());
    Ok(resource)
  }
}

fn read_log_object(heap: &mut Heap, global: vm_js::GcObject) -> Result<Vec<String>> {
  let mut scope = heap.scope();
  scope
    .push_root(Value::Object(global))
    .map_err(|e| Error::Other(e.to_string()))?;

  let log_obj = match get_data_prop(&mut scope, global, "__log") {
    Value::Object(obj) => obj,
    _ => return Err(Error::Other("__log missing".to_string())),
  };
  scope
    .push_root(Value::Object(log_obj))
    .map_err(|e| Error::Other(e.to_string()))?;

  let len = match get_data_prop(&mut scope, global, "__log_len") {
    Value::Number(n) => n as u32,
    _ => return Err(Error::Other("__log_len missing".to_string())),
  };

  let mut out = Vec::with_capacity(len as usize);
  for idx in 0..len {
    let key_s = scope
      .alloc_string(&idx.to_string())
      .map_err(|e| Error::Other(e.to_string()))?;
    scope
      .push_root(Value::String(key_s))
      .map_err(|e| Error::Other(e.to_string()))?;
    let key = PropertyKey::from_string(key_s);
    let value = scope
      .heap()
      .object_get_own_data_property_value(log_obj, &key)
      .map_err(|e| Error::Other(e.to_string()))?
      .unwrap_or(Value::Undefined);
    out.push(get_string(scope.heap(), value));
  }
  Ok(out)
}

struct FetchOnlyHost {
  host_ctx: (),
  window: WindowRealm,
  _fetch_bindings: fastrender::js::WindowFetchBindings,
}

impl WindowRealmHost for FetchOnlyHost {
  fn vm_host_and_window_realm(&mut self) -> Result<(&mut dyn VmHost, &mut WindowRealm)> {
    Ok((&mut self.host_ctx, &mut self.window))
  }
}

fn parse_and_exec_streaming_html(
  html: &str,
  document_url: &str,
  host: &mut WindowHostState,
  event_loop: &mut EventLoop<WindowHostState>,
) -> Result<()> {
  let mut parser = StreamingHtmlParser::new(Some(document_url));
  parser.push_str(html);
  parser.set_eof();

  loop {
    match parser.pump()? {
      StreamingParserYield::Script {
        script,
        base_url_at_this_point,
      } => {
        let source_text = {
          let doc = parser
            .document()
            .expect("document should be available while parsing");
          inline_script_text(&doc, script)
        };

        // Update the JS realm's base URL so `fetch("rel")` uses the document base URL at the time
        // the script runs.
        host
          .window_mut()
          .set_base_url(base_url_at_this_point.clone());
        host.exec_script_with_name_in_event_loop(event_loop, "<inline script>", source_text)?;
      }
      StreamingParserYield::NeedMoreInput => {
        return Err(Error::Other(
          "StreamingHtmlParser unexpectedly requested more input after EOF".to_string(),
        ))
      }
      StreamingParserYield::Finished { .. } => {
        // Keep any queued tasks consistent with the final `<base href>` result.
        host.window_mut().set_base_url(parser.current_base_url());
        break;
      }
    }
  }

  Ok(())
}

#[test]
fn fetch_resolves_relative_url_against_document_base_href() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://ex/base/a", b"ok", 200));
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://ex/doc.html",
    fetcher.clone(),
  )?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  parse_and_exec_streaming_html(
    r#"<!doctype html><head><base href="https://ex/base/"></head><body><script>fetch("a");</script></body>"#,
    "https://ex/doc.html",
    &mut host,
    &mut event_loop,
  )?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.request_urls(),
    vec!["https://ex/base/a".to_string()]
  );
  Ok(())
}

#[test]
fn fetch_promise_callbacks_can_mutate_dom_and_receive_real_vm_host() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/res", b"ok", 200));
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = WindowHostState::from_renderer_dom_with_fetcher(
    &renderer_dom,
    "https://example.com/",
    fetcher,
  )?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  install_assert_non_dummy_vm_host(&mut host)?;

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
fetch("https://example.com/res").then(() => {
  __fastrender_assert_vm_host();
  const d = document.createElement('div');
  d.id = 'fetch';
  document.body.appendChild(d);
});
"#,
  )?;

  assert!(
    host.dom().get_element_by_id("fetch").is_none(),
    "element should not exist before running the event loop"
  );
  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 25,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle,
    "expected event loop to go idle after resolving fetch() promise"
  );
  assert!(
    host.dom().get_element_by_id("fetch").is_some(),
    "expected fetch() Promise callback to mutate the host DOM"
  );
  Ok(())
}

#[test]
fn fetch_base_url_updates_between_scripts() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new()
      .with_response("https://ex/a", b"ok1", 200)
      .with_response("https://ex/base/a", b"ok2", 200),
  );
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://ex/doc.html",
    fetcher.clone(),
  )?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  parse_and_exec_streaming_html(
    r#"<!doctype html><head>
      <script>fetch("a");</script>
      <base href="https://ex/base/">
      <script>fetch("a");</script>
    </head>"#,
    "https://ex/doc.html",
    &mut host,
    &mut event_loop,
  )?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.request_urls(),
    vec!["https://ex/a".to_string(), "https://ex/base/a".to_string()]
  );
  Ok(())
}

#[test]
fn window_fetch_text_orders_microtasks_before_networking() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/x", b"hello", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
 globalThis.__log = {};
 globalThis.__log_len = 0;
  queueMicrotask(() => {
    globalThis.__log[globalThis.__log_len] = "micro";
    globalThis.__log_len = globalThis.__log_len + 1;
  });
   fetch("https://example.com/x")
    .then(r => r.text())
    .then(t => {
      globalThis.__log[globalThis.__log_len] = t;
      globalThis.__log_len = globalThis.__log_len + 1;
    });
  globalThis.__log[globalThis.__log_len] = "sync";
  globalThis.__log_len = globalThis.__log_len + 1;
  "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let log = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    read_log_object(heap, global)?
  };

  assert_eq!(log, vec!["sync", "micro", "hello"]);
  Ok(())
}

#[test]
fn window_fetch_forwards_request_headers() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
 fetch("https://example.com/headers", { headers: { "x-test": "1" } })
   .then(() => {});
 "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(
    fetcher
      .last_request_headers()
      .iter()
      .any(|(name, value)| name == "x-test" && value == "1"),
    "expected ResourceFetcher::fetch_http_request to receive x-test: 1"
  );
  Ok(())
}

#[test]
fn window_fetch_accepts_request_object_input() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
 const req = new Request("https://example.com/headers", { headers: { "x-test": "1" } });
 fetch(req).then(() => {});
 "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(
    fetcher
      .last_request_headers()
      .iter()
      .any(|(name, value)| name == "x-test" && value == "1"),
    "expected Request object input to forward x-test: 1"
  );
  Ok(())
}

#[test]
fn window_request_constructor_clones_request_input() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
 const req1 = new Request("https://example.com/headers", { headers: { "x-test": "1" } });
 const req2 = new Request(req1);
 req2.headers.set("x-test", "2");
 fetch(req2).then(() => {});
 "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(
    fetcher
      .last_request_headers()
      .iter()
      .any(|(name, value)| name == "x-test" && value == "2"),
    "expected Request cloned from Request(input) to forward updated x-test: 2"
  );
  Ok(())
}

#[test]
fn window_fetch_forwards_request_body() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/submit", b"ok", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
 fetch("https://example.com/submit", { method: "POST", body: "payload" }).then(() => {});
 "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.last_request_body(),
    Some(b"payload".to_vec()),
    "expected fetch init body to reach the ResourceFetcher"
  );
  Ok(())
}

#[test]
fn window_fetch_forwards_request_body_from_request_object() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/submit", b"ok", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
 const req = new Request("https://example.com/submit", { method: "POST", body: "payload" });
 fetch(req).then(() => {});
  "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.last_request_body(),
    Some(b"payload".to_vec()),
    "expected Request body to reach the ResourceFetcher when passed to fetch()"
  );
  Ok(())
}

#[test]
fn window_fetch_forwards_request_credentials_mode() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/creds", b"ok", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  fetch("https://example.com/creds", { credentials: "include" }).then(() => {});
  "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.last_request_credentials_mode(),
    Some(FetchCredentialsMode::Include),
    "expected fetch init credentials to reach the ResourceFetcher"
  );
  Ok(())
}

#[test]
fn window_fetch_forwards_request_credentials_mode_from_request_object() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/creds", b"ok", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  const req = new Request("https://example.com/creds", { credentials: "omit" });
  fetch(req).then(() => {});
  "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    fetcher.last_request_credentials_mode(),
    Some(FetchCredentialsMode::Omit),
    "expected Request constructor credentials to reach the ResourceFetcher when passed to fetch()"
  );
  Ok(())
}

#[test]
fn window_fetch_response_json_parses_body() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(InMemoryFetcher::new().with_response(
    "https://example.com/json",
    br#"{"ok": true}"#,
    200,
  ));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  fetch("https://example.com/json")
    .then(r => r.json())
    .then(v => globalThis.__json_ok = v.ok);
 "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let json_ok = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    get_data_prop(&mut scope, global, "__json_ok")
  };
  assert_eq!(json_ok, Value::Bool(true));
  Ok(())
}

#[test]
fn window_fetch_response_array_buffer_returns_bytes() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(InMemoryFetcher::new().with_response(
    "https://example.com/bytes",
    b"\x00\x01\x02\xff",
    200,
  ));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  globalThis.__bytes_err = null;
  globalThis.__ab_byte_length = -1;
  globalThis.__u_is_view = false;
  globalThis.__u_len = -1;
  globalThis.__u_byte_length = -1;
  globalThis.__u_byte_offset = -1;
  globalThis.__u_same_buffer = false;
  globalThis.__u0 = -1;
  globalThis.__u1 = -1;
  globalThis.__u2 = -1;
  globalThis.__u3 = -1;
  globalThis.__u_slice_len = -1;
  globalThis.__u_slice0 = -1;
  globalThis.__u_slice1 = -1;
  globalThis.__u_slice_same_buffer = true;
  globalThis.__ab_slice_byte_length = -1;
  globalThis.__ab_slice0 = -1;
  globalThis.__ab_slice1 = -1;
  fetch("https://example.com/bytes")
    .then(function (r) { return r.arrayBuffer(); })
    .then(function (ab) {
      globalThis.__ab_byte_length = ab.byteLength;
      var u = new Uint8Array(ab);
      globalThis.__u_is_view = ArrayBuffer.isView(u);
      globalThis.__u_len = u.length;
      globalThis.__u_byte_length = u.byteLength;
      globalThis.__u_byte_offset = u.byteOffset;
      globalThis.__u_same_buffer = (u.buffer === ab);
      globalThis.__u0 = u[0];
      globalThis.__u1 = u[1];
      globalThis.__u2 = u[2];
      globalThis.__u3 = u[3];

      var u2 = u.slice(1, 3);
      globalThis.__u_slice_len = u2.length;
      globalThis.__u_slice0 = u2[0];
      globalThis.__u_slice1 = u2[1];
      globalThis.__u_slice_same_buffer = (u2.buffer === ab);

      var ab2 = ab.slice(1, 3);
      globalThis.__ab_slice_byte_length = ab2.byteLength;
      var u3 = new Uint8Array(ab2);
      globalThis.__ab_slice0 = u3[0];
      globalThis.__ab_slice1 = u3[1];
    })
    .catch(function (e) { globalThis.__bytes_err = e && e.name; });
 "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let (
    err,
    ab_byte_length,
    u_is_view,
    u_len,
    u_byte_length,
    u_byte_offset,
    u_same_buffer,
    u0,
    u1,
    u2,
    u3,
    u_slice_len,
    u_slice0,
    u_slice1,
    u_slice_same_buffer,
    ab_slice_byte_length,
    ab_slice0,
    ab_slice1,
  ) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    (
      get_data_prop(&mut scope, global, "__bytes_err"),
      get_data_prop(&mut scope, global, "__ab_byte_length"),
      get_data_prop(&mut scope, global, "__u_is_view"),
      get_data_prop(&mut scope, global, "__u_len"),
      get_data_prop(&mut scope, global, "__u_byte_length"),
      get_data_prop(&mut scope, global, "__u_byte_offset"),
      get_data_prop(&mut scope, global, "__u_same_buffer"),
      get_data_prop(&mut scope, global, "__u0"),
      get_data_prop(&mut scope, global, "__u1"),
      get_data_prop(&mut scope, global, "__u2"),
      get_data_prop(&mut scope, global, "__u3"),
      get_data_prop(&mut scope, global, "__u_slice_len"),
      get_data_prop(&mut scope, global, "__u_slice0"),
      get_data_prop(&mut scope, global, "__u_slice1"),
      get_data_prop(&mut scope, global, "__u_slice_same_buffer"),
      get_data_prop(&mut scope, global, "__ab_slice_byte_length"),
      get_data_prop(&mut scope, global, "__ab_slice0"),
      get_data_prop(&mut scope, global, "__ab_slice1"),
    )
  };

  assert_eq!(err, Value::Null);
  assert_eq!(ab_byte_length, Value::Number(4.0));
  assert_eq!(u_is_view, Value::Bool(true));
  assert_eq!(u_len, Value::Number(4.0));
  assert_eq!(u_byte_length, Value::Number(4.0));
  assert_eq!(u_byte_offset, Value::Number(0.0));
  assert_eq!(u_same_buffer, Value::Bool(true));
  assert_eq!(u0, Value::Number(0.0));
  assert_eq!(u1, Value::Number(1.0));
  assert_eq!(u2, Value::Number(2.0));
  assert_eq!(u3, Value::Number(255.0));
  assert_eq!(u_slice_len, Value::Number(2.0));
  assert_eq!(u_slice0, Value::Number(1.0));
  assert_eq!(u_slice1, Value::Number(2.0));
  assert_eq!(u_slice_same_buffer, Value::Bool(false));
  assert_eq!(ab_slice_byte_length, Value::Number(2.0));
  assert_eq!(ab_slice0, Value::Number(1.0));
  assert_eq!(ab_slice1, Value::Number(2.0));
  Ok(())
}

#[test]
fn window_fetch_response_array_buffer_rejects_second_consumption() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(InMemoryFetcher::new().with_response(
    "https://example.com/once-bytes",
    b"\x00\x01\x02\xff",
    200,
  ));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  globalThis.__ab_first_len = -1;
  globalThis.__ab_first0 = -1;
  globalThis.__ab_second_err = "";
  fetch("https://example.com/once-bytes")
    .then(function (r) {
      return r.arrayBuffer().then(function (b) {
        var u = new Uint8Array(b);
        globalThis.__ab_first_len = u.length;
        globalThis.__ab_first0 = u[0];
        return r.arrayBuffer().then(
          function () { globalThis.__ab_second_err = "no error"; },
          function (e) { globalThis.__ab_second_err = e && e.name; }
        );
      });
    });
 "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let (first_len, first0, second_err) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let first_len = get_data_prop(&mut scope, global, "__ab_first_len");
    let first0 = get_data_prop(&mut scope, global, "__ab_first0");
    let second_err = get_data_prop(&mut scope, global, "__ab_second_err");
    (first_len, first0, get_string(scope.heap(), second_err))
  };

  assert_eq!(first_len, Value::Number(4.0));
  assert_eq!(first0, Value::Number(0.0));
  assert_eq!(second_err, "TypeError");
  Ok(())
}

#[test]
fn array_buffer_and_uint8_array_basic_semantics() -> Result<()> {
  let mut realm = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new("https://example.com/"),
    js_opts_for_test(),
  )
  .map_err(|e| Error::Other(e.to_string()))?;

  let res = realm.exec_script(
    r#"
  globalThis.__ab_byte_length = -1;
  globalThis.__is_view_u = false;
  globalThis.__is_view_ab = true;
  globalThis.__u_len = -1;
  globalThis.__u_byte_length = -1;
  globalThis.__u_byte_offset = -1;
  globalThis.__u_same_buffer = false;
  globalThis.__u0 = -1;
  globalThis.__u1 = -1;
  globalThis.__u2 = -1;
  globalThis.__u3 = -1;

  globalThis.__u_off_byte_offset = -1;
  globalThis.__u_off_len = -1;
  globalThis.__u_off0 = -1;
  globalThis.__u_off1 = -1;
  globalThis.__u_off_same_buffer = false;

  globalThis.__ab_slice_byte_length = -1;
  globalThis.__ab_slice0 = -1;
  globalThis.__ab_slice1 = -1;
  globalThis.__u_slice_len = -1;
  globalThis.__u_slice0 = -1;
  globalThis.__u_slice1 = -1;
  globalThis.__u_slice_same_buffer = true;

  var ab = new ArrayBuffer(4);
  globalThis.__ab_byte_length = ab.byteLength;

  var u = new Uint8Array(ab);
  globalThis.__is_view_u = ArrayBuffer.isView(u);
  globalThis.__is_view_ab = ArrayBuffer.isView(ab);
  globalThis.__u_len = u.length;
  globalThis.__u_byte_length = u.byteLength;
  globalThis.__u_byte_offset = u.byteOffset;
  globalThis.__u_same_buffer = (u.buffer === ab);

  u[0] = 1;
  u[1] = 256;
  u[2] = -1;
  u[3] = 2.9;

  globalThis.__u0 = u[0];
  globalThis.__u1 = u[1];
  globalThis.__u2 = u[2];
  globalThis.__u3 = u[3];

  var u_off = new Uint8Array(ab, 1, 2);
  globalThis.__u_off_byte_offset = u_off.byteOffset;
  globalThis.__u_off_len = u_off.length;
  globalThis.__u_off0 = u_off[0];
  globalThis.__u_off1 = u_off[1];
  globalThis.__u_off_same_buffer = (u_off.buffer === ab);

  var ab_slice = ab.slice(1, 3);
  globalThis.__ab_slice_byte_length = ab_slice.byteLength;
  var u_ab_slice = new Uint8Array(ab_slice);
  globalThis.__ab_slice0 = u_ab_slice[0];
  globalThis.__ab_slice1 = u_ab_slice[1];

  var u_slice = u.slice(1, 3);
  globalThis.__u_slice_len = u_slice.length;
  globalThis.__u_slice0 = u_slice[0];
  globalThis.__u_slice1 = u_slice[1];
  globalThis.__u_slice_same_buffer = (u_slice.buffer === ab);
"#,
  );
  if let Err(err) = res {
    let (_vm, heap) = realm.vm_and_heap_mut();
    return Err(Error::Other(format_vm_error(heap, err)));
  }

  let (
    ab_byte_length,
    is_view_u,
    is_view_ab,
    u_len,
    u_byte_length,
    u_byte_offset,
    u_same_buffer,
    u0,
    u1,
    u2,
    u3,
    u_off_byte_offset,
    u_off_len,
    u_off0,
    u_off1,
    u_off_same_buffer,
    ab_slice_byte_length,
    ab_slice0,
    ab_slice1,
    u_slice_len,
    u_slice0,
    u_slice1,
    u_slice_same_buffer,
  ) = {
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    (
      get_data_prop(&mut scope, global, "__ab_byte_length"),
      get_data_prop(&mut scope, global, "__is_view_u"),
      get_data_prop(&mut scope, global, "__is_view_ab"),
      get_data_prop(&mut scope, global, "__u_len"),
      get_data_prop(&mut scope, global, "__u_byte_length"),
      get_data_prop(&mut scope, global, "__u_byte_offset"),
      get_data_prop(&mut scope, global, "__u_same_buffer"),
      get_data_prop(&mut scope, global, "__u0"),
      get_data_prop(&mut scope, global, "__u1"),
      get_data_prop(&mut scope, global, "__u2"),
      get_data_prop(&mut scope, global, "__u3"),
      get_data_prop(&mut scope, global, "__u_off_byte_offset"),
      get_data_prop(&mut scope, global, "__u_off_len"),
      get_data_prop(&mut scope, global, "__u_off0"),
      get_data_prop(&mut scope, global, "__u_off1"),
      get_data_prop(&mut scope, global, "__u_off_same_buffer"),
      get_data_prop(&mut scope, global, "__ab_slice_byte_length"),
      get_data_prop(&mut scope, global, "__ab_slice0"),
      get_data_prop(&mut scope, global, "__ab_slice1"),
      get_data_prop(&mut scope, global, "__u_slice_len"),
      get_data_prop(&mut scope, global, "__u_slice0"),
      get_data_prop(&mut scope, global, "__u_slice1"),
      get_data_prop(&mut scope, global, "__u_slice_same_buffer"),
    )
  };

  assert_eq!(ab_byte_length, Value::Number(4.0));
  assert_eq!(is_view_u, Value::Bool(true));
  assert_eq!(is_view_ab, Value::Bool(false));
  assert_eq!(u_len, Value::Number(4.0));
  assert_eq!(u_byte_length, Value::Number(4.0));
  assert_eq!(u_byte_offset, Value::Number(0.0));
  assert_eq!(u_same_buffer, Value::Bool(true));
  assert_eq!(u0, Value::Number(1.0));
  assert_eq!(u1, Value::Number(0.0));
  assert_eq!(u2, Value::Number(255.0));
  assert_eq!(u3, Value::Number(2.0));
  assert_eq!(u_off_byte_offset, Value::Number(1.0));
  assert_eq!(u_off_len, Value::Number(2.0));
  assert_eq!(u_off0, Value::Number(0.0));
  assert_eq!(u_off1, Value::Number(255.0));
  assert_eq!(u_off_same_buffer, Value::Bool(true));
  assert_eq!(ab_slice_byte_length, Value::Number(2.0));
  assert_eq!(ab_slice0, Value::Number(0.0));
  assert_eq!(ab_slice1, Value::Number(255.0));
  assert_eq!(u_slice_len, Value::Number(2.0));
  assert_eq!(u_slice0, Value::Number(0.0));
  assert_eq!(u_slice1, Value::Number(255.0));
  assert_eq!(u_slice_same_buffer, Value::Bool(false));
  Ok(())
}

#[test]
fn window_fetch_rejects_on_cors_failure() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://other.example/res", b"ok", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://client.example/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  globalThis.__cors = "";
  fetch("https://other.example/res")
    .then(function () { globalThis.__cors = "resolved"; })
    .catch(function (e) { globalThis.__cors = e && e.name; });
  "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let cors = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let value = get_data_prop(&mut scope, global, "__cors");
    get_string(scope.heap(), value)
  };
  assert_eq!(cors, "TypeError");
  Ok(())
}

#[test]
fn window_fetch_rejects_when_response_body_exceeds_limit() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://client.example/large", b"abcd", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<FetchOnlyHost>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  let document_url = "https://client.example/";
  let mut window = WindowRealm::new_with_js_execution_options(
    WindowRealmConfig::new(document_url),
    js_opts_for_test(),
  )
  .map_err(|e| Error::Other(e.to_string()))?;
  let limits = WebFetchLimits {
    max_response_body_bytes: 3,
    ..WebFetchLimits::default()
  };
  let fetch_bindings = {
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    fastrender::js::install_window_fetch_bindings_with_guard::<FetchOnlyHost>(
      vm,
      realm,
      heap,
      WindowFetchEnv::for_document(fetcher, Some(document_url.to_string())).with_limits(limits),
    )
    .map_err(|e| Error::Other(e.to_string()))?
  };
  let mut host = FetchOnlyHost {
    host_ctx: (),
    window,
    _fetch_bindings: fetch_bindings,
  };

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    let mut hooks = VmJsEventLoopHooks::<FetchOnlyHost>::new_with_host(host)?;
    hooks.set_event_loop(event_loop);
    let (host_ctx, realm) = host.vm_host_and_window_realm()?;
    realm.reset_interrupt();
    let res = realm.exec_script_with_host_and_hooks(
      host_ctx,
      &mut hooks,
      r#"
  globalThis.__size_err_name = "";
  globalThis.__size_err_msg = "";
  fetch("https://client.example/large")
    .then(function () { globalThis.__size_err_name = "resolved"; })
    .catch(function (e) {
      globalThis.__size_err_name = e && e.name;
      globalThis.__size_err_msg = e && e.message;
    });
  "#,
    );
    if let Some(err) = hooks.finish(realm.heap_mut()) {
      return Err(err);
    }
    if let Err(err) = res {
      let (_vm, heap) = realm.vm_and_heap_mut();
      return Err(Error::Other(format_vm_error(heap, err)));
    }
    Ok(())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let realm = host.window_realm()?;
  let (name, msg) = {
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let name = get_data_prop(&mut scope, global, "__size_err_name");
    let msg = get_data_prop(&mut scope, global, "__size_err_msg");
    (get_string(scope.heap(), name), get_string(scope.heap(), msg))
  };
  assert_eq!(name, "TypeError");
  assert!(
    msg.contains("response body exceeds configured limits"),
    "unexpected error message: {msg}"
  );
  Ok(())
}

#[test]
fn window_fetch_response_text_rejects_second_consumption() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/once-text", b"hello", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  globalThis.__text1 = "";
  globalThis.__text2_err = "";
  globalThis.__text_body_used = false;
  fetch("https://example.com/once-text")
    .then(function (r) {
      return r.text().then(function (t) {
        globalThis.__text1 = t;
        globalThis.__text_body_used = r.bodyUsed;
        return r.text().then(
          function () { globalThis.__text2_err = "no error"; },
          function (e) { globalThis.__text2_err = e && e.name; }
        );
      });
    });
  "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let (text1, text2_err, body_used) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let text1 = get_data_prop(&mut scope, global, "__text1");
    let text2_err = get_data_prop(&mut scope, global, "__text2_err");
    let body_used = get_data_prop(&mut scope, global, "__text_body_used");
    (
      get_string(scope.heap(), text1),
      get_string(scope.heap(), text2_err),
      body_used,
    )
  };

  assert_eq!(text1, "hello");
  assert_eq!(text2_err, "TypeError");
  assert_eq!(body_used, Value::Bool(true));
  Ok(())
}

#[test]
fn window_fetch_accepts_request_object() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/headers2", b"ok", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  let req = new Request("https://example.com/headers2", { headers: { "x-test": "2" } });
  fetch(req).then(() => {});
  "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert!(
    fetcher
      .last_request_headers()
      .iter()
      .any(|(name, value)| name == "x-test" && value == "2"),
    "expected fetch(Request) to forward headers to ResourceFetcher::fetch_http_request"
  );
  Ok(())
}

#[test]
fn window_response_clone_duplicates_body() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    Arc::new(InMemoryFetcher::new()),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  globalThis.__clone_text = "";
  let r = new Response("hello");
  let c = r.clone();
  r.text().then(function (t1) {
    return c.text().then(function (t2) {
      globalThis.__clone_text = t1 + "," + t2;
    });
  });
  "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let clone_text = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let value = get_data_prop(&mut scope, global, "__clone_text");
    get_string(scope.heap(), value)
  };
  assert_eq!(clone_text, "hello,hello");
  Ok(())
}

#[test]
fn window_response_clone_throws_when_body_used() -> Result<()> {
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    Arc::new(InMemoryFetcher::new()),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host
      .exec_script_in_event_loop(
        event_loop,
        r#"
  globalThis.__clone_error = "";
  let r = new Response("hello");
  r.text().then(() => {
    try {
      r.clone();
      globalThis.__clone_error = "no error";
    } catch (e) {
      globalThis.__clone_error = e.name;
    }
  });
  "#,
      )
      .map(|_| ())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let clone_error = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let value = get_data_prop(&mut scope, global, "__clone_error");
    get_string(scope.heap(), value)
  };
  assert_eq!(clone_error, "TypeError");
  Ok(())
}

#[test]
fn window_xhr_supports_sync_send_and_with_credentials() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/data", b"hello", 200));
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
  globalThis.__events = "";
  globalThis.__status = 0;
  globalThis.__text = "";
  const xhr = new XMLHttpRequest();
  xhr.onreadystatechange = () => { globalThis.__events += "rs:" + xhr.readyState + ","; };
  xhr.onload = () => { globalThis.__events += "load,"; };
  xhr.onerror = () => { globalThis.__events += "error,"; };
  xhr.onloadend = () => { globalThis.__events += "loadend,"; };
  xhr.withCredentials = true;
  xhr.open("GET", "https://example.com/data", false);
  xhr.send();
  globalThis.__status = xhr.status;
  globalThis.__text = xhr.responseText;
  "#,
  )?;

  let (events, status, text) = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    let events = get_data_prop(&mut scope, global, "__events");
    let status = get_data_prop(&mut scope, global, "__status");
    let text = get_data_prop(&mut scope, global, "__text");
    (
      get_string(scope.heap(), events),
      status,
      get_string(scope.heap(), text),
    )
  };

  assert_eq!(events, "rs:1,rs:2,rs:3,rs:4,load,loadend,");
  assert_eq!(status, Value::Number(200.0));
  assert_eq!(text, "hello");
  assert_eq!(
    fetcher.last_request_credentials_mode(),
    Some(FetchCredentialsMode::Include),
    "expected XHR withCredentials=true to map to FetchCredentialsMode::Include"
  );
  Ok(())
}

#[test]
fn window_xhr_open_undefined_async_defaults_to_true() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> =
    Arc::new(InMemoryFetcher::new().with_response("https://example.com/data", b"hello", 200));
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = host_state_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    host.exec_script_in_event_loop(
      event_loop,
      r#"
  globalThis.__log = "";
  const xhr = new XMLHttpRequest();
  xhr.onloadend = () => { globalThis.__log += "loadend"; };
  xhr.open("GET", "https://example.com/data", undefined);
  xhr.send();
  globalThis.__log += "after_send";
  "#,
    )?;
    Ok(())
  })?;

  assert_eq!(
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let log = {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global)).unwrap();
    let value = get_data_prop(&mut scope, global, "__log");
    get_string(scope.heap(), value)
  };
  assert_eq!(log, "after_sendloadend");
  Ok(())
}

#[test]
fn readable_stream_pipe_to_writable_stream_fulfills() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__chunks = [];
globalThis.__closed = false;
globalThis.__done = false;
globalThis.__err = "";

const rs = new ReadableStream({
  start(controller) {
    controller.enqueue("a");
    controller.enqueue("b");
    controller.close();
  }
});

const ws = new WritableStream({
  write(chunk) { globalThis.__chunks.push(chunk); },
  close() { globalThis.__closed = true; },
});

rs.pipeTo(ws)
  .then(() => { globalThis.__done = true; })
  .catch((e) => { globalThis.__err = String(e && e.message || e); });
"#,
  )?;

  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle
  );

  let ok = host.exec_script_in_event_loop(
    &mut event_loop,
    "globalThis.__done === true && globalThis.__err === '' && globalThis.__closed === true && globalThis.__chunks.join(',') === 'a,b'",
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn readable_stream_pipe_to_prevent_close_does_not_close_writer() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__chunks = [];
globalThis.__closed = false;
globalThis.__done = false;
globalThis.__err = "";

const rs = new ReadableStream({
  start(controller) {
    controller.enqueue("x");
    controller.close();
  }
});

const ws = new WritableStream({
  write(chunk) { globalThis.__chunks.push(chunk); },
  close() { globalThis.__closed = true; },
});

rs.pipeTo(ws, { preventClose: true })
  .then(() => { globalThis.__done = true; })
  .catch((e) => { globalThis.__err = String(e && e.message || e); });
"#,
  )?;

  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 10,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle
  );

  let ok = host.exec_script_in_event_loop(
    &mut event_loop,
    "globalThis.__done === true && globalThis.__err === '' && globalThis.__closed === false && globalThis.__chunks.join(',') === 'x'",
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn readable_stream_pipe_to_rejects_when_writer_write_throws() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__unhandled = false;
globalThis.__caught = false;
globalThis.__err = "";

window.addEventListener("unhandledrejection", () => { globalThis.__unhandled = true; });

const rs = new ReadableStream({
  start(controller) {
    controller.enqueue("x");
    controller.close();
  }
});

const ws = new WritableStream({
  write() { throw new Error("boom"); },
});

rs.pipeTo(ws)
  .catch((e) => {
    globalThis.__caught = true;
    globalThis.__err = String(e && e.message || e);
  });
"#,
  )?;

  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 25,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle
  );

  let ok = host.exec_script_in_event_loop(
    &mut event_loop,
    "globalThis.__caught === true && globalThis.__err === 'boom' && globalThis.__unhandled === false",
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn readable_stream_pipe_to_rejects_invalid_options_signal_primitive() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__unhandled = false;
globalThis.__threw = false;
globalThis.__rejected = false;
globalThis.__fulfilled = false;
globalThis.__err_name = "";
globalThis.__err_msg = "";

window.addEventListener("unhandledrejection", () => { globalThis.__unhandled = true; });

const rs = new ReadableStream({
  start(controller) {
    controller.enqueue("x");
    controller.close();
  }
});

const ws = new WritableStream({
  write() {},
  close() {},
});

try {
  const p = rs.pipeTo(ws, { signal: 1 });
  p.then(() => { globalThis.__fulfilled = true; })
    .catch((e) => {
      globalThis.__rejected = true;
      globalThis.__err_name = e && e.name;
      globalThis.__err_msg = String(e && e.message || e);
    });
} catch (e) {
  globalThis.__threw = true;
  globalThis.__err_name = e && e.name;
  globalThis.__err_msg = String(e && e.message || e);
}
"#,
  )?;

  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 25,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle
  );

  let ok = host.exec_script_in_event_loop(
    &mut event_loop,
    "globalThis.__unhandled === false &&\n\
     globalThis.__fulfilled === false &&\n\
     (globalThis.__threw === true || globalThis.__rejected === true) &&\n\
     globalThis.__err_name === 'TypeError' &&\n\
     String(globalThis.__err_msg || '').includes('AbortSignal')",
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn readable_stream_pipe_to_rejects_invalid_options_signal_object() -> Result<()> {
  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><html><head></head><body></body></html>")?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);

  host.exec_script_in_event_loop(
    &mut event_loop,
    r#"
globalThis.__unhandled = false;
globalThis.__threw = false;
globalThis.__rejected = false;
globalThis.__fulfilled = false;
globalThis.__err_name = "";
globalThis.__err_msg = "";

window.addEventListener("unhandledrejection", () => { globalThis.__unhandled = true; });

const rs = new ReadableStream({
  start(controller) {
    controller.enqueue("x");
    controller.close();
  }
});

const ws = new WritableStream({
  write() {},
  close() {},
});

try {
  const p = rs.pipeTo(ws, { signal: {} });
  p.then(() => { globalThis.__fulfilled = true; })
    .catch((e) => {
      globalThis.__rejected = true;
      globalThis.__err_name = e && e.name;
      globalThis.__err_msg = String(e && e.message || e);
    });
} catch (e) {
  globalThis.__threw = true;
  globalThis.__err_name = e && e.name;
  globalThis.__err_msg = String(e && e.message || e);
}
"#,
  )?;

  assert_eq!(
    event_loop.run_until_idle(
      &mut host,
      RunLimits {
        max_tasks: 25,
        max_microtasks: 100,
        max_wall_time: None,
      },
    )?,
    RunUntilIdleOutcome::Idle
  );

  let ok = host.exec_script_in_event_loop(
    &mut event_loop,
    "globalThis.__unhandled === false &&\n\
     globalThis.__fulfilled === false &&\n\
     (globalThis.__threw === true || globalThis.__rejected === true) &&\n\
     globalThis.__err_name === 'TypeError' &&\n\
     String(globalThis.__err_msg || '').includes('AbortSignal')",
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn element_get_bounding_client_rect_returns_zero_dom_rect_in_window_host_state() -> Result<()> {
  let renderer_dom = fastrender::dom::parse_html(
    "<!doctype html><html><body><div id=x></div></body></html>",
  )?;
  let mut host = host_state_from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let mut event_loop = EventLoop::<WindowHostState>::new();

  let ok = host.exec_script_in_event_loop(
    &mut event_loop,
    r#"(() => {
      const el = document.getElementById('x');
      if (typeof Element.prototype.getBoundingClientRect !== 'function') return false;
      if (typeof el.getBoundingClientRect !== 'function') return false;
      const r = el.getBoundingClientRect();
      if (!(r instanceof DOMRectReadOnly)) return false;
      const props = ['x', 'y', 'width', 'height', 'top', 'left', 'right', 'bottom'];
      for (const p of props) { if (typeof r[p] !== 'number') return false; }
      if (!(r.x === 0 && r.y === 0 && r.width === 0 && r.height === 0)) return false;
      let msg = null;
      try { Element.prototype.getBoundingClientRect.call(document); } catch (e) { msg = e && e.message; }
      return msg === 'Illegal invocation';
    })()"#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}
