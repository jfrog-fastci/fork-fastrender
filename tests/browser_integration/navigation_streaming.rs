use base64::Engine as _;
use fastrender::dom2::{Document, NodeId};
use fastrender::js::{EventLoop, HtmlScriptId, RunLimits, RunUntilIdleOutcome, ScriptElementSpec};
use fastrender::{
  BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, ModuleScriptExecutionStatus, RenderOptions,
  Result,
};
use std::sync::{Arc, Mutex};

use super::support::ExecutorWithWindow;

fn has_element_by_id(dom: &Document, target: &str) -> bool {
  let mut stack = vec![dom.root()];
  while let Some(id) = stack.pop() {
    if dom.id(id).ok().flatten() == Some(target) {
      return true;
    }
    let node = dom.node(id);
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  false
}

#[derive(Clone)]
struct LoggingExecutor {
  log: Arc<Mutex<Vec<String>>>,
}

impl BrowserTabJsExecutor for LoggingExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let code = script_text.trim();
    match code {
      "HEAD_INLINE" => {
        self.log.lock().unwrap().push("head-inline".to_string());
      }
      "CHECK_PARTIAL" => {
        let dom = document.dom();
        let before = has_element_by_id(dom, "before");
        let after = has_element_by_id(dom, "after");
        let eof = has_element_by_id(dom, "eof");
        self
          .log
          .lock()
          .unwrap()
          .push(format!("partial:before={before} after={after} eof={eof}"));
      }
      "EXT_BLOCKING" => {
        self.log.lock().unwrap().push("ext-blocking".to_string());
      }
      "EXT_ASYNC" => {
        self.log.lock().unwrap().push("ext-async".to_string());
      }
      "EXT_ASYNC_INTERLEAVE" => {
        let after = has_element_by_id(document.dom(), "after");
        self
          .log
          .lock()
          .unwrap()
          .push(format!("ext-async-interleave:after={after}"));
      }
      "EXT_ASYNC_MUTATE" => {
        let (after, head) = {
          let dom = document.dom();
          let after = has_element_by_id(dom, "after");
          let head = dom.head().expect("expected document to have <head>");
          (after, head)
        };
        document
          .dom_mut()
          .set_attribute(head, "data-from-async", "1")
          .expect("set head attribute");
        self
          .log
          .lock()
          .unwrap()
          .push(format!("ext-async-mutate:after={after}"));
      }
      "EXT_DEFER" => {
        let eof = has_element_by_id(document.dom(), "eof");
        self
          .log
          .lock()
          .unwrap()
          .push(format!("ext-defer:eof={eof}"));
      }
      other => {
        self.log.lock().unwrap().push(format!("script:{other}"));
      }
    }
    Ok(())
  }

  fn execute_module_script(
    &mut self,
    _script_id: HtmlScriptId,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<ModuleScriptExecutionStatus> {
    self.execute_classic_script(script_text, spec, current_script, document, event_loop)?;
    Ok(ModuleScriptExecutionStatus::Completed)
  }
}

#[test]
fn browser_tab_navigate_to_url_uses_streaming_parser_and_script_scheduling() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let log = Arc::new(Mutex::new(Vec::<String>::new()));
  let executor = LoggingExecutor {
    log: Arc::clone(&log),
  };
  let options = RenderOptions::default();

  // Start from an empty tab so we can exercise URL navigation code paths.
  let mut tab = BrowserTab::from_html("", options.clone(), ExecutorWithWindow::new(executor))?;

  tab.register_script_source("https://example.com/blocking.js", "EXT_BLOCKING");
  tab.register_script_source("https://example.com/async.js", "EXT_ASYNC");
  tab.register_script_source("https://example.com/defer.js", "EXT_DEFER");

  let html = r#"<!doctype html>
  <html>
    <head>
      <script>HEAD_INLINE</script>
      <script src="https://example.com/blocking.js"></script>
      <script async src="https://example.com/async.js"></script>
      <script defer src="https://example.com/defer.js"></script>
    </head>
    <body>
      <div id="before"></div>
      <script>CHECK_PARTIAL</script>
      <div id="after"></div>
      <div id="eof"></div>
    </body>
  </html>"#;

  let encoded = base64::engine::general_purpose::STANDARD.encode(html.as_bytes());
  let url = format!("data:text/html;base64,{encoded}");

  tab.navigate_to_url(&url, options)?;

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let log = log.lock().unwrap().clone();
  assert_eq!(
    log,
    vec![
      "head-inline".to_string(),
      "ext-blocking".to_string(),
      "ext-async".to_string(),
      "partial:before=true after=false eof=false".to_string(),
      "ext-defer:eof=true".to_string(),
    ]
  );

  Ok(())
}

#[test]
fn browser_tab_async_script_executes_before_parse_completes() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let log = Arc::new(Mutex::new(Vec::<String>::new()));
  let executor = LoggingExecutor {
    log: Arc::clone(&log),
  };
  let options = RenderOptions::default();

  // Use a large `<!-- ... -->` comment so the default parse budget (64 pump iterations) exhausts
  // before the `#after` marker is parsed, without creating expensive-to-layout text nodes.
  let big_comment = "a".repeat(520 * 1024);
  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <script async src="https://example.com/async.js"></script>
        <!--{big_comment}-->
      </head>
      <body>
        <div id="after"></div>
      </body>
    </html>"#
  );

  let mut tab = BrowserTab::from_html("", options.clone(), executor)?;
  tab.register_script_source("https://example.com/async.js", "EXT_ASYNC_INTERLEAVE");

  tab.navigate_to_html(&html, options)?;

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let log = log.lock().unwrap().clone();
  assert_eq!(log, vec!["ext-async-interleave:after=false".to_string()]);

  assert!(has_element_by_id(tab.dom(), "after"));

  Ok(())
}

#[test]
fn browser_tab_async_script_mutation_persists_across_parse_slices() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let log = Arc::new(Mutex::new(Vec::<String>::new()));
  let executor = LoggingExecutor {
    log: Arc::clone(&log),
  };
  let options = RenderOptions::default();

  let mut tab = BrowserTab::from_html("", options.clone(), executor)?;
  tab.register_script_source("https://example.com/async.js", "EXT_ASYNC_MUTATE");

  // Use a tiny budget so parsing is forced to yield and resume via event-loop tasks.
  let mut js_options = tab.js_execution_options();
  js_options.dom_parse_budget = fastrender::js::ParseBudget::new(2);
  tab.set_js_execution_options(js_options);

  let filler = "a".repeat(24 * 1024);
  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <script async src="https://example.com/async.js"></script>
      </head>
      <body>
        <div id="before"></div>
        {filler}
        <div id="after"></div>
      </body>
    </html>"#
  );

  tab.navigate_to_html(&html, options)?;

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let log = log.lock().unwrap().clone();
  assert_eq!(log, vec!["ext-async-mutate:after=false".to_string()]);

  assert!(has_element_by_id(tab.dom(), "after"));
  let head = tab.dom().head().expect("expected head element after parse");
  assert_eq!(
    tab.dom().get_attribute(head, "data-from-async").unwrap(),
    Some("1")
  );

  Ok(())
}
