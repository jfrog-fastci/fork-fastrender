use fastrender::dom::DomNode;
use fastrender::dom2::{Document, NodeId, NodeKind};
use fastrender::js::dom_integration::prepare_dynamic_scripts_on_subtree_insertion;
use fastrender::js::window_timers::VmJsEventLoopHooks;
use fastrender::js::{
  install_window_animation_frame_bindings, install_window_timers_bindings, ClassicScriptScheduler,
  DomHost, EventLoop, JsExecutionOptions, RunLimits, RunUntilIdleOutcome, ScriptElementEvent,
  ScriptElementSpec, ScriptEventDispatcher, ScriptExecutor, ScriptLoader, ScriptType, VirtualClock,
  WindowHostState, WindowRealm, WindowRealmHost,
};
use fastrender::resource::HttpFetcher;
use fastrender::{Error, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use vm_js::{GcObject, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, VmError};

const JS_BOOTSTRAP: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;

  g.__log = {};
  g.__log_len = 0;

  if (!g.console) g.console = {};
  g.console.log = function () {
    // vm-js does not currently implement Array.prototype.push/join, so avoid any array helpers
    // here. Keep this compatible with minimal builtins so tests can depend on console.log.
    var s = "";
    for (var i = 0; i < arguments.length; i++) {
      if (i) s += " ";
      s += String(arguments[i]);
    }
    g.__log[g.__log_len] = s;
    g.__log_len = g.__log_len + 1;
  };

  // `WindowHostState` installs Fetch bindings that are specialized for `WindowHostState` as the
  // event-loop host type. This harness executes JS with a different host type, so remove these
  // globals to prevent accidental UB from calling them.
  g.fetch = undefined;
  g.Headers = undefined;
  g.Request = undefined;
  g.Response = undefined;
  g.XMLHttpRequest = undefined;
})();
"#;

#[derive(Default)]
struct ScriptLoaderState {
  sources: HashMap<String, String>,
  next_handle: usize,
  handles_by_url: HashMap<String, usize>,
  completed: VecDeque<(usize, String)>,
}

pub struct HostState {
  window: WindowHostState,
  loader: ScriptLoaderState,
}

impl HostState {
  fn exec_script_in_event_loop(
    &mut self,
    event_loop: &mut EventLoop<HostState>,
    source: &str,
  ) -> Result<Value> {
    let (vm_host, window) = self.vm_host_and_window_realm();
    window.reset_interrupt();
    let mut hooks = VmJsEventLoopHooks::<HostState>::new(&mut *vm_host);
    hooks.set_event_loop(event_loop);
    let result = window.exec_script_with_host_and_hooks(vm_host, &mut hooks, source);
    if let Some(err) = hooks.finish(window.heap_mut()) {
      return Err(err);
    }
    match result {
      Ok(value) => Ok(value),
      Err(err) => Err(Error::Other(format_vm_error(window.heap_mut(), err))),
    }
  }

  fn complete_external_script(&mut self, url: &str) -> Result<()> {
    let handle = *self
      .loader
      .handles_by_url
      .get(url)
      .ok_or_else(|| Error::Other(format!("no pending script load for url={url}")))?;
    let src = self
      .loader
      .sources
      .get(url)
      .cloned()
      .ok_or_else(|| Error::Other(format!("no registered script source for url={url}")))?;
    self.loader.completed.push_back((handle, src));
    Ok(())
  }
}

impl DomHost for HostState {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Document) -> R,
  {
    self.window.with_dom(f)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Document) -> (R, bool),
  {
    self.window.mutate_dom(f)
  }
}

impl WindowRealmHost for HostState {
  fn vm_host_and_window_realm(&mut self) -> (&mut dyn vm_js::VmHost, &mut WindowRealm) {
    self.window.vm_host_and_window_realm()
  }
}

impl ScriptLoader for HostState {
  type Handle = usize;

  fn load_blocking(
    &mut self,
    url: &str,
    _destination: fastrender::resource::FetchDestination,
    _credentials_mode: fastrender::resource::FetchCredentialsMode,
  ) -> Result<String> {
    self
      .loader
      .sources
      .get(url)
      .cloned()
      .ok_or_else(|| Error::Other(format!("no registered script source for url={url}")))
  }

  fn start_load(
    &mut self,
    url: &str,
    _destination: fastrender::resource::FetchDestination,
    _credentials_mode: fastrender::resource::FetchCredentialsMode,
  ) -> Result<Self::Handle> {
    let handle = self.loader.next_handle;
    self.loader.next_handle += 1;
    self.loader.handles_by_url.insert(url.to_string(), handle);
    Ok(handle)
  }

  fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>> {
    Ok(self.loader.completed.pop_front())
  }
}

fn node_root_is_shadow_root(dom: &Document, mut node: NodeId) -> bool {
  loop {
    match &dom.node(node).kind {
      NodeKind::ShadowRoot { .. } => return true,
      NodeKind::Document { .. } => return false,
      _ => {}
    }
    let Some(parent) = dom.node(node).parent else {
      return false;
    };
    node = parent;
  }
}

impl ScriptExecutor for HostState {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    // The HTML "execute the script block" algorithm is only observable via `document.currentScript`
    // in our unit tests. We keep the bookkeeping minimal: set currentScript to the executing script
    // element for classic scripts in the document tree, and always restore afterward.
    let state = self.window.document_host().current_script_handle().clone();

    let mut new_current: Option<NodeId> = None;

    if let Some(script_node) = spec.node_id {
      let (connected_for_scripting, in_shadow_root) = self.with_dom(|dom| {
        (
          dom.is_connected_for_scripting(script_node),
          node_root_is_shadow_root(dom, script_node),
        )
      });

      if !connected_for_scripting {
        return Ok(());
      }

      if spec.script_type == ScriptType::Classic && !in_shadow_root {
        new_current = Some(script_node);
      }
    }

    let prev_current = state.borrow().current_script;
    state.borrow_mut().current_script = new_current;

    let result = self.exec_script_in_event_loop(event_loop, script_text);

    state.borrow_mut().current_script = prev_current;
    result.map(|_| ())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    // The JS harness uses QuickJS evaluation directly. Treat module scripts as evaluating the
    // provided (already-resolved) source text while relying on the host `ScriptOrchestrator` to
    // handle `document.currentScript` bookkeeping (`null` for module scripts).
    self.execute_classic_script(script_text, spec, event_loop)
  }
}

impl ScriptEventDispatcher for HostState {
  fn dispatch_script_event(
    &mut self,
    _event: ScriptElementEvent,
    _spec: &ScriptElementSpec,
  ) -> Result<()> {
    Ok(())
  }
}

pub struct Harness {
  clock: Arc<VirtualClock>,
  host: HostState,
  event_loop: EventLoop<HostState>,
  script_scheduler: ClassicScriptScheduler<HostState>,
}

impl Harness {
  pub fn new(document_url: &str, html: &str) -> Result<Self> {
    let renderer_dom = fastrender::dom::parse_html(html)?;
    let dom = Document::from_renderer_dom(&renderer_dom);

    let mut js_options = JsExecutionOptions::default();
    // The JS harness aims to exercise both classic and module script execution. The production
    // default is `supports_module_scripts=false` for safety (until embedders opt in), so enable
    // module scripts explicitly here.
    js_options.supports_module_scripts = true;
    // Unit tests can run many JS harness cases concurrently; keep the per-script wall-time budget
    // generous enough to avoid flaky deadline aborts under CPU contention while still bounding
    // hostile `while(true){}` cases.
    js_options.event_loop_run_limits.max_wall_time = Some(Duration::from_secs(5));

    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::<HostState>::with_clock(clock.clone());

    let mut host = HostState {
      window: WindowHostState::new_with_fetcher_and_clock_and_options(
        dom,
        document_url.to_string(),
        Arc::new(HttpFetcher::new()),
        clock.clone(),
        js_options,
      )?,
      loader: ScriptLoaderState::default(),
    };

    // `WindowHostState` installs global bindings specialized for `WindowHostState`. Re-install the
    // timer bindings for this harness's host type so `event_loop_mut_from_hooks::<HostState>()`
    // resolves to the correct `EventLoop<HostState>`.
    {
      let realm = host.window.window_mut();
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      install_window_timers_bindings::<HostState>(vm, realm_ref, heap)
        .map_err(|e| Error::Other(e.to_string()))?;
      install_window_animation_frame_bindings::<HostState>(vm, realm_ref, heap)
        .map_err(|e| Error::Other(e.to_string()))?;
    }

    let mut this = Self {
      clock,
      host,
      event_loop,
      script_scheduler: ClassicScriptScheduler::with_options(js_options),
    };

    // Initialize logging + disable UB-prone globals.
    this.exec_script(JS_BOOTSTRAP)?;
    this.take_log();

    Ok(this)
  }

  pub fn exec_script(&mut self, src: &str) -> Result<()> {
    self
      .host
      .exec_script_in_event_loop(&mut self.event_loop, src)
      .map_err(|e| Error::Other(format!("exec_script failed: {e}")))?;
    self
      .event_loop
      .perform_microtask_checkpoint(&mut self.host)
      .map_err(|e| Error::Other(format!("microtask checkpoint failed: {e}")))?;
    self
      .prepare_dynamic_scripts()
      .map_err(|e| Error::Other(format!("dynamic script scan failed: {e}")))?;
    Ok(())
  }

  pub fn advance_time(&mut self, ms: u64) {
    self.clock.advance(Duration::from_millis(ms));
  }

  pub fn run_until_idle(&mut self, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    let scheduler = &mut self.script_scheduler;
    self
      .event_loop
      .run_until_idle_with_hook(&mut self.host, limits, |host, event_loop| {
        let root = host.with_dom(|dom| dom.root());
        let base_url = host.window.base_url.clone();
        prepare_dynamic_scripts_on_subtree_insertion(
          host,
          scheduler,
          event_loop,
          root,
          base_url.as_deref(),
        )
      })
  }

  pub fn host_mut(&mut self) -> &mut HostState {
    &mut self.host
  }

  pub fn event_loop_mut(&mut self) -> &mut EventLoop<HostState> {
    &mut self.event_loop
  }

  pub fn host_and_event_loop_mut(&mut self) -> (&mut HostState, &mut EventLoop<HostState>) {
    (&mut self.host, &mut self.event_loop)
  }

  pub fn snapshot_dom(&self) -> DomNode {
    self.host.with_dom(|dom| dom.to_renderer_dom())
  }

  pub fn take_log(&mut self) -> Vec<String> {
    let realm = self.host.window.window_mut();
    let global = realm.global_object();
    let (_vm, heap) = realm.vm_and_heap_mut();
    let log = read_log_object(heap, global).expect("failed to read js_harness log");
    reset_log_object(heap, global).expect("failed to reset js_harness log");
    log
  }

  pub fn set_external_script_sources(&mut self, sources: HashMap<String, String>) {
    self.host.loader.sources = sources;
  }

  pub fn load_external_script_blocking(&mut self, url: &str) -> Result<String> {
    self
      .host
      .loader
      .sources
      .get(url)
      .cloned()
      .ok_or_else(|| Error::Other(format!("no registered script source for url={url}")))
  }

  /// Start a deterministic "async" external script load.
  ///
  /// Call [`Harness::complete_external_script`] to resolve it, and
  /// [`Harness::poll_external_script_completion`] to consume completions in the chosen order.
  pub fn start_external_script_load(&mut self, url: &str) -> Result<usize> {
    let handle = self.host.loader.next_handle;
    self.host.loader.next_handle += 1;
    self
      .host
      .loader
      .handles_by_url
      .insert(url.to_string(), handle);
    Ok(handle)
  }

  pub fn poll_external_script_completion(&mut self) -> Result<Option<(usize, String)>> {
    Ok(self.host.loader.completed.pop_front())
  }

  pub fn complete_external_script_only(&mut self, url: &str) -> Result<()> {
    self.host.complete_external_script(url)
  }

  pub fn complete_external_script(&mut self, url: &str) -> Result<()> {
    self.host.complete_external_script(url)?;
    self
      .script_scheduler
      .poll(&mut self.host, &mut self.event_loop)?;
    Ok(())
  }

  fn prepare_dynamic_scripts(&mut self) -> Result<()> {
    let root = self.host.with_dom(|dom| dom.root());
    let base_url = self.host.window.base_url.clone();
    prepare_dynamic_scripts_on_subtree_insertion(
      &mut self.host,
      &mut self.script_scheduler,
      &mut self.event_loop,
      root,
      base_url.as_deref(),
    )
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

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> std::result::Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn get_string(heap: &Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn get_data_prop(scope: &mut Scope<'_>, obj: vm_js::GcObject, name: &str) -> Value {
  let key_s = scope.alloc_string(name).unwrap();
  let key = PropertyKey::from_string(key_s);
  scope
    .heap()
    .object_get_own_data_property_value(obj, &key)
    .unwrap()
    .unwrap_or(Value::Undefined)
}

fn read_log_object(heap: &mut Heap, global: GcObject) -> Result<Vec<String>> {
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

fn reset_log_object(heap: &mut Heap, global: GcObject) -> Result<()> {
  let mut scope = heap.scope();
  scope
    .push_root(Value::Object(global))
    .map_err(|e| Error::Other(e.to_string()))?;

  let log_obj = scope
    .alloc_object()
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::Object(log_obj))
    .map_err(|e| Error::Other(e.to_string()))?;

  let log_key = alloc_key(&mut scope, "__log").map_err(|e| Error::Other(e.to_string()))?;
  scope
    .define_property(global, log_key, data_desc(Value::Object(log_obj)))
    .map_err(|e| Error::Other(e.to_string()))?;

  let len_key = alloc_key(&mut scope, "__log_len").map_err(|e| Error::Other(e.to_string()))?;
  scope
    .define_property(global, len_key, data_desc(Value::Number(0.0)))
    .map_err(|e| Error::Other(e.to_string()))?;
  Ok(())
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
      return match (name, message) {
        (Some(name), Some(message)) if !message.is_empty() => format!("{name}: {message}"),
        (Some(name), _) => name,
        (_, Some(message)) => message,
        _ => "uncaught exception".to_string(),
      };
    }
    return "uncaught exception".to_string();
  }

  match err {
    VmError::Syntax(diags) => format!("syntax error: {diags:?}"),
    other => other.to_string(),
  }
}

mod dynamic_scripts;
mod script_scheduler;
mod smoke;
mod timers;
