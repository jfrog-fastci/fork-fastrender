use fastrender::api::{
  BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, RenderOptions,
};
use fastrender::dom2::NodeId;
use fastrender::error::Result;
use fastrender::js::{
  CurrentScriptStateHandle, EventLoop, JsExecutionOptions, ScriptElementSpec, TaskSource, WindowRealm, WindowRealmConfig,
  WindowRealmHost, RunLimits,
};
use std::collections::HashSet;

struct ExecutorWithWindow<E> {
  inner: E,
  host_ctx: (),
  window: WindowRealm,
}

impl<E> ExecutorWithWindow<E> {
  fn new(inner: E) -> Self {
    let window =
      WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).expect("create WindowRealm");
    Self {
      inner,
      host_ctx: (),
      window,
    }
  }
}

impl<E: BrowserTabJsExecutor> BrowserTabJsExecutor for ExecutorWithWindow<E> {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_classic_script(script_text, spec, current_script, document, event_loop)
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_module_script(script_text, spec, current_script, document, event_loop)
  }

  fn execute_import_map_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_import_map_script(script_text, spec, current_script, document, event_loop)
  }

  fn reset_for_navigation(
    &mut self,
    document_url: Option<&str>,
    document: &mut BrowserDocumentDom2,
    current_script_state: &CurrentScriptStateHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Result<()> {
    self.inner.reset_for_navigation(
      document_url,
      document,
      current_script_state,
      js_execution_options,
    )
  }

  fn window_realm_mut(&mut self) -> Option<&mut WindowRealm> {
    if let Some(realm) = self.inner.window_realm_mut() {
      Some(realm)
    } else {
      Some(&mut self.window)
    }
  }
}

impl<E> WindowRealmHost for ExecutorWithWindow<E> {
  fn vm_host_and_window_realm(&mut self) -> (&mut dyn vm_js::VmHost, &mut WindowRealm) {
    let ExecutorWithWindow { host_ctx, window, .. } = self;
    (host_ctx, window)
  }
}

#[test]
fn js_tracing_emits_basic_spans_for_scripts_and_tasks() {
  let dir = tempfile::tempdir().expect("tempdir");
  let trace_path = dir.path().join("trace.json");

  let mut options = RenderOptions::default();
  options.trace_output = Some(trace_path.clone());

  let html = r#"<!doctype html>
  <html>
    <head>
      <script type="importmap">{"imports":{}}</script>
      <script>queueMicrotask</script>
      <script type="module">queueTask</script>
      <script async src="https://example.com/ext.js"></script>
    </head>
  </html>"#;

  struct DummyExecutor;

  impl BrowserTabJsExecutor for DummyExecutor {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      if script_text.contains("queueMicrotask") {
        event_loop.queue_microtask(|_host, _event_loop| Ok(()))?;
      }
      if script_text.contains("queueTask") {
        event_loop.queue_task(TaskSource::DOMManipulation, |_host, _event_loop| Ok(()))?;
      }
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      spec: &ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.execute_classic_script(script_text, spec, current_script, document, event_loop)
    }
  }

  let mut js_options = JsExecutionOptions::default();
  js_options.supports_module_scripts = true;
  let mut tab = BrowserTab::from_html_with_js_execution_options(
    html,
    options,
    ExecutorWithWindow::new(DummyExecutor),
    js_options,
  )
  .expect("create tab");
  tab.register_script_source("https://example.com/ext.js", "queueTask");
  let _ = tab
    .run_event_loop_until_idle(RunLimits::unbounded())
    .expect("run event loop");
  tab.write_trace().expect("write trace");

  let raw = std::fs::read_to_string(&trace_path).expect("read trace");
  let json: serde_json::Value = serde_json::from_str(&raw).expect("parse json");
  let events = json
    .get("traceEvents")
    .and_then(|v| v.as_array())
    .expect("traceEvents array");

  let mut names: Vec<&str> = Vec::new();
  let mut script_execute_types: HashSet<String> = HashSet::new();
  let mut script_fetch_type = None;
  let mut script_fetch_destination = None;
  let mut script_fetch_credentials_mode = None;
  let mut script_fetch_async_attr = None;
  let mut script_fetch_defer_attr = None;
  let mut script_fetch_parser_inserted = None;
  for event in events {
    if let Some(name) = event.get("name").and_then(|v| v.as_str()) {
      names.push(name);
      if name == "js.script.execute" {
        if let Some(ty) = event
          .get("args")
          .and_then(|args| args.get("script_type"))
          .and_then(|v| v.as_str())
        {
          script_execute_types.insert(ty.to_string());
        }
      }
      if name == "js.script.fetch" {
        script_fetch_destination = event
          .get("args")
          .and_then(|args| args.get("destination"))
          .and_then(|v| v.as_str())
          .or(script_fetch_destination);
        script_fetch_credentials_mode = event
          .get("args")
          .and_then(|args| args.get("credentials_mode"))
          .and_then(|v| v.as_str())
          .or(script_fetch_credentials_mode);
        script_fetch_async_attr = event
          .get("args")
          .and_then(|args| args.get("async_attr"))
          .and_then(|v| v.as_bool())
          .or(script_fetch_async_attr);
        script_fetch_defer_attr = event
          .get("args")
          .and_then(|args| args.get("defer_attr"))
          .and_then(|v| v.as_bool())
          .or(script_fetch_defer_attr);
        script_fetch_parser_inserted = event
          .get("args")
          .and_then(|args| args.get("parser_inserted"))
          .and_then(|v| v.as_bool())
          .or(script_fetch_parser_inserted);
        script_fetch_type = event
          .get("args")
          .and_then(|args| args.get("script_type"))
          .and_then(|v| v.as_str())
          .or(script_fetch_type);
      }
    }
  }

  assert!(
    names.contains(&"js.script.fetch"),
    "expected js.script.fetch span; got names={names:?}"
  );
  assert!(
    names.contains(&"js.script.execute"),
    "expected js.script.execute span; got names={names:?}"
  );
  assert!(
    names.contains(&"js.task.run"),
    "expected js.task.run span; got names={names:?}"
  );
  assert!(
    names.contains(&"js.microtask_checkpoint"),
    "expected js.microtask_checkpoint span; got names={names:?}"
  );

  assert_eq!(
    script_fetch_type,
    Some("classic"),
    "expected js.script.fetch span to include args.script_type=classic"
  );
  assert_eq!(
    script_fetch_destination,
    Some("script"),
    "expected js.script.fetch span to include args.destination=script"
  );
  assert_eq!(
    script_fetch_credentials_mode,
    Some("include"),
    "expected js.script.fetch span to include args.credentials_mode=include"
  );
  assert_eq!(
    script_fetch_async_attr,
    Some(true),
    "expected js.script.fetch span to include args.async_attr=true"
  );
  assert_eq!(
    script_fetch_defer_attr,
    Some(false),
    "expected js.script.fetch span to include args.defer_attr=false"
  );
  assert_eq!(
    script_fetch_parser_inserted,
    Some(true),
    "expected js.script.fetch span to include args.parser_inserted=true"
  );
  assert!(
    script_execute_types.contains("classic"),
    "expected js.script.execute spans to include classic script execution; got {script_execute_types:?}"
  );
  assert!(
    script_execute_types.contains("module"),
    "expected js.script.execute spans to include module script execution; got {script_execute_types:?}"
  );
  assert!(
    script_execute_types.contains("importmap"),
    "expected js.script.execute spans to include importmap execution; got {script_execute_types:?}"
  );
}
