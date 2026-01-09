use fastrender::dom2::{Document as Dom2Document, NodeId, NodeKind};
use fastrender::js::runtime::with_event_loop;
use fastrender::js::{
  EventLoop, RunLimits, RunUntilIdleOutcome, ScriptBlockExecutor, ScriptOrchestrator, ScriptType, TaskSource,
  VirtualClock, WindowHostState, WindowRealm, WindowRealmConfig,
};
use fastrender::resource::{
  FetchCredentialsMode, FetchDestination, FetchRequest, FetchedResource, HttpRequest, ResourceFetcher,
};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vm_js::{Heap, PropertyKey, Scope, Value, Vm, VmError};

fn install_vm_js_microtask_checkpoint_hook(event_loop: &mut EventLoop<WindowHostState>) {
  fn drain(host: &mut WindowHostState, event_loop: &mut EventLoop<WindowHostState>) -> Result<()> {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      realm.reset_interrupt();
      let (vm, heap) = realm.vm_and_heap_mut();
      vm.perform_microtask_checkpoint(heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      Ok(())
    })
  }

  event_loop.set_microtask_checkpoint_hook(Some(drain));
}

fn get_string(heap: &Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn format_vm_error(heap: &mut Heap, err: VmError) -> String {
  match err {
    VmError::Throw(value) => {
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
    }
    VmError::Syntax(diags) => format!("syntax error: {diags:?}"),
    other => other.to_string(),
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

fn find_script_elements(dom: &Dom2Document) -> Vec<NodeId> {
  dom
    .subtree_preorder(dom.root())
    .filter(|&id| matches!(&dom.node(id).kind, NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("script")))
    .collect()
}

fn get_current_script(vm: &mut Vm, heap: &mut Heap, document_obj: vm_js::GcObject) -> Result<Value> {
  let mut scope = heap.scope();
  let key_s = scope.alloc_string("currentScript").map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::String(key_s))
    .map_err(|e| Error::Other(e.to_string()))?;
  let key = PropertyKey::from_string(key_s);
  vm.get(&mut scope, document_obj, key)
    .map_err(|e| Error::Other(e.to_string()))
}

fn get_wrapper_node_id(
  vm: &mut Vm,
  heap: &mut Heap,
  wrapper: vm_js::GcObject,
) -> Result<usize> {
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
    return Err(Error::Other("expected __fastrender_node_id to be a number".to_string()));
  };
  Ok(n as usize)
}

#[test]
fn window_self_and_document_url_are_exposed() -> Result<()> {
  let url = "https://example.com/";
  let mut realm = WindowRealm::new(WindowRealmConfig::new(url)).map_err(|e| Error::Other(e.to_string()))?;

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
fn document_current_script_tracks_sequential_classic_scripts() -> Result<()> {
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
      let realm = host.window_mut();
      let global = realm.global_object();
      let (vm, heap) = realm.vm_and_heap_mut();
      let document_obj = {
        let mut scope = heap.scope();
        let Value::Object(doc) = get_data_prop(&mut scope, global, "document") else {
          return Err(Error::Other("document is not an object".to_string()));
        };
        doc
      };

      let value = get_current_script(vm, heap, document_obj)?;
      let Value::Object(wrapper) = value else {
        return Err(Error::Other("expected document.currentScript to be an object".to_string()));
      };
      let node_id = get_wrapper_node_id(vm, heap, wrapper)?;
      self.observed.push(node_id);
      Ok(())
    }
  }

  let renderer_dom =
    fastrender::dom::parse_html("<!doctype html><script></script><script></script>")?;
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;
  let scripts = find_script_elements(host.dom());
  assert_eq!(scripts.len(), 2);

  let mut orchestrator = ScriptOrchestrator::new();
  let mut executor = RecordingExecutor::default();

  // Outside execution, currentScript should be null.
  {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let document_obj = {
      let mut scope = heap.scope();
      let Value::Object(doc) = get_data_prop(&mut scope, global, "document") else {
        return Err(Error::Other("document is not an object".to_string()));
      };
      doc
    };
    let value = get_current_script(vm, heap, document_obj)?;
    assert_eq!(value, Value::Null);
  }

  orchestrator.execute_script_element(
    &mut host,
    scripts[0],
    ScriptType::Classic,
    &mut executor,
  )?;
  {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let document_obj = {
      let mut scope = heap.scope();
      let Value::Object(doc) = get_data_prop(&mut scope, global, "document") else {
        return Err(Error::Other("document is not an object".to_string()));
      };
      doc
    };
    let value = get_current_script(vm, heap, document_obj)?;
    assert_eq!(value, Value::Null);
  }

  orchestrator.execute_script_element(
    &mut host,
    scripts[1],
    ScriptType::Classic,
    &mut executor,
  )?;
  {
    let realm = host.window_mut();
    let global = realm.global_object();
    let (vm, heap) = realm.vm_and_heap_mut();
    let document_obj = {
      let mut scope = heap.scope();
      let Value::Object(doc) = get_data_prop(&mut scope, global, "document") else {
        return Err(Error::Other("document is not an object".to_string()));
      };
      doc
    };
    let value = get_current_script(vm, heap, document_obj)?;
    assert_eq!(value, Value::Null);
  }

  assert_eq!(
    executor.observed,
    vec![scripts[0].index(), scripts[1].index()]
  );
  Ok(())
}

#[test]
fn location_href_setter_errors_deterministically() -> Result<()> {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))
    .map_err(|e| Error::Other(e.to_string()))?;

  let global = realm.global_object();
  let (vm, heap) = realm.vm_and_heap_mut();
  let mut scope = heap.scope();

  let location = get_data_prop(&mut scope, global, "location");
  let Value::Object(location_obj) = location else {
    panic!("expected location to be an object");
  };

  let href_key_s = scope.alloc_string("href").map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::String(href_key_s))
    .map_err(|e| Error::Other(e.to_string()))?;
  let href_key = PropertyKey::from_string(href_key_s);

  let new_url_s = scope
    .alloc_string("https://example.com/next")
    .map_err(|e| Error::Other(e.to_string()))?;
  let new_value = Value::String(new_url_s);

  let err = scope
    .ordinary_set(vm, location_obj, href_key, new_value, Value::Object(location_obj))
    .expect_err("expected location.href setter to fail");
  assert!(
    matches!(err, VmError::TypeError(msg) if msg == "Navigation via location.href is not implemented yet"),
    "unexpected error: {err:?}"
  );
  Ok(())
}

#[test]
fn js_execution_can_observe_window_globals() -> Result<()> {
  let url = "https://example.com/path";
  let mut realm = WindowRealm::new(WindowRealmConfig::new(url))
    .map_err(|e| Error::Other(e.to_string()))?;

  let value = realm
    .exec_script("window === globalThis && self === window")
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
fn location_url_components_are_exposed_to_js_execution() -> Result<()> {
  let url = "https://example.com:8080/path/to/page?query=1#hash";
  let mut realm = WindowRealm::new(WindowRealmConfig::new(url))
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
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  {
    let realm = host.window_mut();
    let res = realm.exec_script(
      r#"
  globalThis.__head_id = document.head.id;
  globalThis.__body_id = document.body.id;
  globalThis.__head_same = document.head === document.head;
  globalThis.__body_same = document.body === document.body;
  document.body.id = "new";
  globalThis.__body_id_after = document.body.id;
  "#,
    );
    if let Err(err) = res {
      let (_vm, heap) = realm.vm_and_heap_mut();
      return Err(Error::Other(format_vm_error(heap, err)));
    }
  }

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
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;

  {
    let realm = host.window_mut();
    let res = realm.exec_script(
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
    );
    if let Err(err) = res {
      let (_vm, heap) = realm.vm_and_heap_mut();
      return Err(Error::Other(format_vm_error(heap, err)));
    }
  }

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
fn document_current_script_is_visible_to_js_execution() -> Result<()> {
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
      let realm = host.window_mut();

      let stable = realm
        .exec_script("document.currentScript === document.currentScript")
        .map_err(|e| Error::Other(e.to_string()))?;
      let Value::Bool(stable) = stable else {
        return Err(Error::Other(
          "expected document.currentScript identity check to return a bool".to_string(),
        ));
      };
      self.wrapper_identity_ok.push(stable);

      let node_id = realm
        .exec_script("document.currentScript.__fastrender_node_id")
        .map_err(|e| Error::Other(e.to_string()))?;
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
  let mut host = WindowHostState::from_renderer_dom(&renderer_dom, "https://example.com/")?;
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

  orchestrator.execute_script_element(
    &mut host,
    scripts[0],
    ScriptType::Classic,
    &mut executor,
  )?;
  orchestrator.execute_script_element(
    &mut host,
    scripts[1],
    ScriptType::Classic,
    &mut executor,
  )?;

  assert_eq!(executor.wrapper_identity_ok, vec![true, true]);
  assert_eq!(executor.observed, vec![scripts[0].index(), scripts[1].index()]);
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
  last_request_headers: Mutex<Vec<(String, String)>>,
  last_request_body: Mutex<Option<Vec<u8>>>,
  last_request_credentials_mode: Mutex<Option<FetchCredentialsMode>>,
}

impl InMemoryFetcher {
  fn new() -> Self {
    Self {
      routes: HashMap::new(),
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

#[test]
fn window_fetch_text_orders_microtasks_before_networking() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/x", b"hello", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 globalThis.__log = {};
 globalThis.__log_len = 0;
 queueMicrotask(() => {
   globalThis.__log[globalThis.__log_len] = "micro";
   globalThis.__log_len = globalThis.__log_len + 1;
 });
  fetch("https://example.com/x")
    .then(function (r) { return r.text(); })
    .then(function (t) {
      globalThis.__log[globalThis.__log_len] = t;
      globalThis.__log_len = globalThis.__log_len + 1;
    });
 globalThis.__log[globalThis.__log_len] = "sync";
 globalThis.__log_len = globalThis.__log_len + 1;
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 fetch("https://example.com/headers", { headers: { "x-test": "1" } })
   .then(() => {});
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 const req = new Request("https://example.com/headers", { headers: { "x-test": "1" } });
 fetch(req).then(() => {});
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/headers", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 const req1 = new Request("https://example.com/headers", { headers: { "x-test": "1" } });
 const req2 = new Request(req1);
 req2.headers.set("x-test", "2");
 fetch(req2).then(() => {});
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/submit", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 fetch("https://example.com/submit", { method: "POST", body: "payload" }).then(() => {});
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/submit", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
 const req = new Request("https://example.com/submit", { method: "POST", body: "payload" });
 fetch(req).then(() => {});
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/creds", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  fetch("https://example.com/creds", { credentials: "include" }).then(() => {});
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/creds", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  const req = new Request("https://example.com/creds", { credentials: "omit" });
  fetch(req).then(() => {});
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/json", br#"{"ok": true}"#, 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher,
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  fetch("https://example.com/json")
    .then(function (r) { return r.json(); })
    .then(function (v) { globalThis.__json_ok = v.ok; });
 "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
fn window_fetch_accepts_request_object() -> Result<()> {
  let fetcher: Arc<InMemoryFetcher> = Arc::new(
    InMemoryFetcher::new().with_response("https://example.com/headers2", b"ok", 200),
  );
  let clock = Arc::new(VirtualClock::new());
  let mut event_loop = EventLoop::<WindowHostState>::with_clock(clock);
  install_vm_js_microtask_checkpoint_hook(&mut event_loop);
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    fetcher.clone(),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
        r#"
  let req = new Request("https://example.com/headers2", { headers: { "x-test": "2" } });
  fetch(req).then(() => {});
  "#,
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    Arc::new(InMemoryFetcher::new()),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
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
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
  let mut host = WindowHostState::new_with_fetcher(
    Dom2Document::new(QuirksMode::NoQuirks),
    "https://example.com/",
    Arc::new(InMemoryFetcher::new()),
  )?;

  event_loop.queue_task(TaskSource::Script, |host, event_loop| {
    with_event_loop(event_loop, || {
      let realm = host.window_mut();
      let res = realm.exec_script(
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
      );
      if let Err(err) = res {
        let (_vm, heap) = realm.vm_and_heap_mut();
        return Err(Error::Other(format_vm_error(heap, err)));
      }
      Ok(())
    })
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
