use fastrender::api::{
  BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, RenderOptions,
};
use fastrender::dom2::NodeId;
use fastrender::error::Result;
use fastrender::js::{
  CurrentScriptStateHandle, EventLoop, JsExecutionOptions, ScriptElementSpec, TaskSource, WindowRealm, WindowRealmConfig,
  WindowRealmHost, RunLimits,
};

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
      <script>queueMicrotask</script>
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

  let mut tab = BrowserTab::from_html(html, options, ExecutorWithWindow::new(DummyExecutor)).expect("create tab");
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
  let mut script_execute_type = None;
  let mut script_fetch_type = None;
  for event in events {
    if let Some(name) = event.get("name").and_then(|v| v.as_str()) {
      names.push(name);
      if name == "js.script.execute" {
        script_execute_type = event
          .get("args")
          .and_then(|args| args.get("script_type"))
          .and_then(|v| v.as_str())
          .or(script_execute_type);
      }
      if name == "js.script.fetch" {
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
    script_execute_type,
    Some("classic"),
    "expected js.script.execute span to include args.script_type=classic"
  );
}
