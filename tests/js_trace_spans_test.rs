use fastrender::api::{BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, RenderOptions};
use fastrender::dom2::NodeId;
use fastrender::error::Result;
use fastrender::js::{EventLoop, RunLimits, ScriptElementSpec, TaskSource};

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
  }

  let mut tab = BrowserTab::from_html(html, options, DummyExecutor).expect("create tab");
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
  for event in events {
    if let Some(name) = event.get("name").and_then(|v| v.as_str()) {
      names.push(name);
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
}
