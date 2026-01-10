use fastrender::api::VmJsBrowserTabExecutor;
use fastrender::dom2::{Document, NodeId};
use fastrender::js::{
  Clock, EventLoop, JsExecutionOptions, RunLimits, RunUntilIdleOutcome, TaskSource, VirtualClock, WindowRealm,
  WindowRealmConfig, WindowRealmHost,
};
use fastrender::{
  BrowserTab, BrowserTabHost, BrowserTabJsExecutor, Error, RenderOptions, Result,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::support::rgba_at;
use super::support::TempSite;

fn find_element_by_id(dom: &Document, target: &str) -> Option<NodeId> {
  let mut stack = vec![dom.root()];
  while let Some(id) = stack.pop() {
    if dom.id(id).ok().flatten() == Some(target) {
      return Some(id);
    }
    let node = dom.node(id);
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

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
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_classic_script(script_text, spec, current_script, document, event_loop)
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_module_script(script_text, spec, current_script, document, event_loop)
  }

  fn execute_import_map_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_import_map_script(script_text, spec, current_script, document, event_loop)
  }

  fn reset_for_navigation(
    &mut self,
    document_url: Option<&str>,
    document: &mut fastrender::BrowserDocumentDom2,
    current_script_state: &fastrender::js::CurrentScriptStateHandle,
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

#[derive(Default)]
struct QueuedMutationExecutor;

impl BrowserTabJsExecutor for QueuedMutationExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let code = script_text.trim();
    if code != "queue-mutation" {
      return Ok(());
    }

    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      let box_id = find_element_by_id(host.dom(), "box")
        .ok_or_else(|| Error::Other("expected #box element".to_string()))?;

      {
        let dom = host.dom_mut();
        dom
          .set_attribute(box_id, "class", "b")
          .map_err(|e| Error::Other(e.to_string()))?;
      }

      event_loop.queue_microtask(move |host, _event_loop| {
        let dom = host.dom_mut();
        dom
          .set_attribute(box_id, "data-microtask", "1")
          .map_err(|e| Error::Other(e.to_string()))?;
        Ok(())
      })?;

      Ok(())
    })?;

    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self.execute_classic_script(script_text, spec, current_script, document, event_loop)
  }
}

#[derive(Default)]
struct NoopExecutor;

impl BrowserTabJsExecutor for NoopExecutor {
  fn execute_classic_script(
    &mut self,
    _script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut fastrender::BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self.execute_classic_script(script_text, spec, current_script, document, event_loop)
  }
}

#[test]
fn browser_tab_script_src_uses_base_url_at_discovery() -> Result<()> {
  #[derive(Clone)]
  struct RecordingExecutor {
    executed: Arc<Mutex<Vec<String>>>,
  }

  impl BrowserTabJsExecutor for RecordingExecutor {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &fastrender::js::ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut fastrender::BrowserDocumentDom2,
      _event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .executed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(script_text.to_string());
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      spec: &fastrender::js::ScriptElementSpec,
      current_script: Option<NodeId>,
      document: &mut fastrender::BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self.execute_classic_script(script_text, spec, current_script, document, event_loop)
    }
  }

  let executed = Arc::new(Mutex::new(Vec::<String>::new()));
  let options = RenderOptions::new().with_viewport(64, 64);
  let html = r#"<!doctype html>
    <html>
      <head>
        <script async src="a.js"></script>
        <base href="https://ex/base/">
      </head>
    </html>"#;

  let mut tab = BrowserTab::from_html(
    html,
    options,
    ExecutorWithWindow::new(RecordingExecutor {
      executed: Arc::clone(&executed),
    }),
  )?;
  tab.register_script_source("a.js", "A");
  tab.register_script_source("https://ex/base/a.js", "B");

  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  let log = executed
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .clone();
  assert_eq!(log, vec!["A".to_string()]);
  Ok(())
}

#[test]
fn browser_tab_parses_with_scripting_enabled_semantics() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          .red { width: 64px; height: 64px; background: rgb(255, 0, 0); }
          .blue { width: 64px; height: 64px; background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <noscript><div class="red"></div></noscript>
        <div class="blue"></div>
       </body>
     </html>"#;

  let options = RenderOptions::new().with_viewport(64, 64);
  let mut tab = BrowserTab::from_html(html, options, ExecutorWithWindow::new(NoopExecutor::default()))?;
  let pixmap = tab.render_frame()?;

  assert_eq!(
    rgba_at(&pixmap, 32, 32),
    [0, 0, 255, 255],
    "expected `<noscript>` content to be suppressed when scripting is enabled"
  );
  Ok(())
}

#[test]
fn browser_tab_runs_queued_tasks_and_microtasks_and_rerenders() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .a { background: rgb(255, 0, 0); }
          .b { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="a"></div>
        <script>queue-mutation</script>
      </body>
    </html>"#;
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_timeout(Some(Duration::from_secs(1)));

  let mut tab =
    BrowserTab::from_html(html, options, ExecutorWithWindow::new(QueuedMutationExecutor::default()))?;
  let frame_a = tab.render_frame()?;
  assert!(tab.render_if_needed()?.is_none());

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let dom = tab.dom();
  let box_id = find_element_by_id(dom, "box").expect("#box id");
  assert_eq!(
    dom
      .class_name(box_id)
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("b")
  );
  assert_eq!(
    dom
      .get_attribute(box_id, "data-microtask")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("1")
  );

  let frame_b = tab
    .render_frame()?;
  assert_ne!(frame_b.data(), frame_a.data(), "expected pixels to change");
  assert!(tab.render_if_needed()?.is_none());
  Ok(())
}

#[derive(Default)]
struct ErrorThenMutationExecutor;

impl BrowserTabJsExecutor for ErrorThenMutationExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let code = script_text.trim();
    if code != "queue-error-then-mutation" {
      return Ok(());
    }

    // First task throws an uncaught exception (should be reported but must not abort the event loop).
    event_loop.queue_task(TaskSource::Script, |_host, _event_loop| {
      Err(Error::Other("boom".to_string()))
    })?;

    // Second task mutates the DOM; this should still execute and be observable in the rendered
    // output even though the first task failed.
    event_loop.queue_task(TaskSource::Script, |host, _event_loop| {
      let box_id = find_element_by_id(host.dom(), "box")
        .ok_or_else(|| Error::Other("expected #box element".to_string()))?;
      host
        .dom_mut()
        .set_attribute(box_id, "class", "b")
        .map_err(|e| Error::Other(e.to_string()))?;
      Ok(())
    })?;

    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self.execute_classic_script(script_text, spec, current_script, document, event_loop)
  }
}

#[test]
fn browser_tab_task_error_does_not_prevent_later_dom_mutations_and_rendering() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .a { background: rgb(255, 0, 0); }
          .b { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="a"></div>
        <script>queue-error-then-mutation</script>
      </body>
    </html>"#;
  let options = RenderOptions::new().with_viewport(64, 64);

  let js_execution_options = JsExecutionOptions {
    event_loop_run_limits: RunLimits::unbounded(),
    ..JsExecutionOptions::default()
  };
  let mut tab = BrowserTab::from_html_with_js_execution_options(
    html,
    options,
    ExecutorWithWindow::new(ErrorThenMutationExecutor::default()),
    js_execution_options,
  )?;

  let frame_a = tab
    .tick_frame()?
    .expect("expected a rendered frame after initial parse task");
  assert_eq!(rgba_at(&frame_a, 32, 32), [255, 0, 0, 255]);

  let outcome = tab.run_until_stable_with_run_limits(RunLimits::unbounded(), 10)?;
  match outcome {
    fastrender::RunUntilStableOutcome::Stable { frames_rendered } => {
      assert!(
        frames_rendered >= 1,
        "expected a new frame to be rendered after the mutation"
      );
    }
    other => panic!("expected stable outcome, got {other:?}"),
  }

  let frame_b = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_b, 32, 32), [0, 0, 255, 255]);
  Ok(())
}

#[derive(Default)]
struct TimerMutationExecutor;

impl BrowserTabJsExecutor for TimerMutationExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let code = script_text.trim();
    if code != "queue-timer-mutation" {
      return Ok(());
    }

    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      let box_id = find_element_by_id(host.dom(), "box")
        .ok_or_else(|| Error::Other("expected #box element".to_string()))?;

      host
        .dom_mut()
        .set_attribute(box_id, "data-phase", "task")
        .map_err(|e| Error::Other(e.to_string()))?;
      host
        .dom_mut()
        .set_attribute(box_id, "class", "task")
        .map_err(|e| Error::Other(e.to_string()))?;

      event_loop.set_timeout(Duration::from_millis(10), move |host, _event_loop| {
        host
          .dom_mut()
          .set_attribute(box_id, "data-phase", "timer")
          .map_err(|e| Error::Other(e.to_string()))?;
        host
          .dom_mut()
          .set_attribute(box_id, "class", "timer")
          .map_err(|e| Error::Other(e.to_string()))?;
        Ok(())
      })?;

      Ok(())
    })?;

    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self.execute_classic_script(script_text, spec, current_script, document, event_loop)
  }
}

#[test]
fn browser_tab_timer_tasks_fire_after_clock_advance_and_rerender() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .start { background: rgb(0, 0, 0); }
          .task { background: rgb(255, 0, 0); }
          .timer { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="start"></div>
        <script>queue-timer-mutation</script>
      </body>
    </html>"#;
  let options = RenderOptions::new().with_viewport(64, 64);

  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock_for_loop);

  let mut tab = BrowserTab::from_html_with_event_loop(
    html,
    options,
    ExecutorWithWindow::new(TimerMutationExecutor::default()),
    event_loop,
  )?;
  let frame_start = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_start, 32, 32), [0, 0, 0, 255]);

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    tab.dom()
      .get_attribute(find_element_by_id(tab.dom(), "box").expect("#box id"), "data-phase")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("task")
  );
  let frame_task = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_task, 32, 32), [255, 0, 0, 255]);

  clock.advance(Duration::from_millis(10));
  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    tab.dom()
      .get_attribute(find_element_by_id(tab.dom(), "box").expect("#box id"), "data-phase")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("timer")
  );
  let frame_timer = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_timer, 32, 32), [0, 0, 255, 255]);
  Ok(())
}

#[derive(Clone)]
struct ParseTimeDomAssertionExecutor {
  log: Arc<Mutex<Vec<String>>>,
}

impl BrowserTabJsExecutor for ParseTimeDomAssertionExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let code = script_text.trim().to_string();
    if code == "assert-partial-dom" {
      assert!(
        find_element_by_id(document.dom(), "after").is_none(),
        "markup after </script> must not be visible when the script executes"
      );
    }
    self
      .log
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(code);
    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self.execute_classic_script(script_text, spec, current_script, document, event_loop)
  }
}

#[test]
fn browser_tab_executes_parser_inserted_scripts_against_partial_dom() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let log = Arc::new(Mutex::new(Vec::<String>::new()));
  let executor = ParseTimeDomAssertionExecutor { log: Arc::clone(&log) };
  let options = RenderOptions::new().with_viewport(1, 1);

  let html = "<!doctype html><script>assert-partial-dom</script><div id=after></div>";
  let tab = BrowserTab::from_html(html, options, ExecutorWithWindow::new(executor))?;

  assert!(
    find_element_by_id(tab.dom(), "after").is_some(),
    "expected parsing to resume after executing the script"
  );

  assert_eq!(
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_slice(),
    &["assert-partial-dom".to_string()]
  );
  Ok(())
}

#[test]
fn browser_tab_with_event_loop_executes_parser_inserted_scripts_against_partial_dom() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let log = Arc::new(Mutex::new(Vec::<String>::new()));
  let executor = ParseTimeDomAssertionExecutor { log: Arc::clone(&log) };
  let options = RenderOptions::new().with_viewport(1, 1);

  let html = "<!doctype html><script>assert-partial-dom</script><div id=after></div>";
  let event_loop = EventLoop::<BrowserTabHost>::new();
  let tab = BrowserTab::from_html_with_event_loop(html, options, ExecutorWithWindow::new(executor), event_loop)?;

  assert!(
    find_element_by_id(tab.dom(), "after").is_some(),
    "expected parsing to resume after executing the script"
  );
  assert_eq!(
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_slice(),
    &["assert-partial-dom".to_string()]
  );
  Ok(())
}

#[test]
fn browser_tab_navigate_to_url_executes_parser_inserted_scripts_against_partial_dom() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let site = TempSite::new();
  site.write("blocking.js", "assert-partial-dom");
  let index_url = site.write(
    "index.html",
    "<!doctype html><script src=\"blocking.js\"></script><div id=after></div>",
  );

  let log = Arc::new(Mutex::new(Vec::<String>::new()));
  let executor = ParseTimeDomAssertionExecutor { log: Arc::clone(&log) };
  let options = RenderOptions::new().with_viewport(1, 1);

  let mut tab = BrowserTab::from_html("", options.clone(), ExecutorWithWindow::new(executor))?;
  tab.navigate_to_url(&index_url, options)?;
  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  assert!(
    find_element_by_id(tab.dom(), "after").is_some(),
    "expected parsing to resume after executing the script"
  );
  assert_eq!(
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_slice(),
    &["assert-partial-dom".to_string()]
  );
  Ok(())
}

#[test]
fn browser_tab_navigate_to_url_resolves_script_src_against_parse_time_base_url() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let site = TempSite::new();

  // Create both root and subdir variants so the test can detect incorrect base resolution.
  site.write("a.js", "A_ROOT");
  site.write("sub/a.js", "A_SUB");
  site.write("b.js", "B_ROOT");
  site.write("sub/b.js", "B_SUB");

  let index_url = site.write(
    "index.html",
    "<!doctype html>\
     <head>\
       <script src=\"a.js\"></script>\
       <base href=\"sub/\">\
       <script src=\"b.js\"></script>\
     </head>",
  );

  let log = Arc::new(Mutex::new(Vec::<String>::new()));
  let executor = ParseTimeDomAssertionExecutor { log: Arc::clone(&log) };
  let options = RenderOptions::new().with_viewport(1, 1);

  let mut tab = BrowserTab::from_html("", options.clone(), ExecutorWithWindow::new(executor))?;
  tab.navigate_to_url(&index_url, options)?;
  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  assert_eq!(
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_slice(),
    &["A_ROOT".to_string(), "B_SUB".to_string()]
  );
  Ok(())
}

#[test]
fn browser_tab_navigate_to_url_honors_async_and_defer_scheduling() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let site = TempSite::new();
  site.write("async.js", "ASYNC");
  site.write("defer.js", "DEFER");

  let index_url = site.write(
    "index.html",
    "<!doctype html>\
     <script src=\"async.js\" async></script>\
     <script src=\"defer.js\" defer></script>",
  );

  let log = Arc::new(Mutex::new(Vec::<String>::new()));
  let executor = ParseTimeDomAssertionExecutor { log: Arc::clone(&log) };
  let options = RenderOptions::new().with_viewport(1, 1);

  let mut tab = BrowserTab::from_html("", options.clone(), ExecutorWithWindow::new(executor))?;
  tab.navigate_to_url(&index_url, options)?;

  assert!(
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_empty(),
    "async/defer scripts should not execute synchronously during parsing"
  );

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_slice(),
    &["ASYNC".to_string(), "DEFER".to_string()]
  );
  Ok(())
}

#[derive(Clone)]
struct NavigateUrlParseTimeExecutor {
  log: Arc<Mutex<Vec<String>>>,
  expected_external_url: String,
}

impl BrowserTabJsExecutor for NavigateUrlParseTimeExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    _current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let code = script_text.trim().to_string();
    match code.as_str() {
      "inline-check" => {
        assert!(
          find_element_by_id(document.dom(), "after-inline").is_none(),
          "markup after inline </script> must not be visible when the inline script executes"
        );
        assert!(
          find_element_by_id(document.dom(), "after-external").is_none(),
          "markup after the external script must not be visible when the inline script executes"
        );
        let body = document
          .dom()
          .body()
          .ok_or_else(|| Error::Other("expected <body> element".to_string()))?;
        document
          .dom_mut()
          .set_attribute(body, "data-inline", "1")
          .map_err(|e| Error::Other(e.to_string()))?;
      }
      "external-check" => {
        assert_eq!(
          spec.src.as_deref(),
          Some(self.expected_external_url.as_str()),
          "expected external script src to resolve against the navigation URL"
        );
        assert!(
          find_element_by_id(document.dom(), "after-inline").is_some(),
          "expected parsing to have progressed past the inline script before the external script executes"
        );
        assert!(
          find_element_by_id(document.dom(), "after-external").is_none(),
          "markup after external </script> must not be visible when the external script executes"
        );
        let body = document
          .dom()
          .body()
          .ok_or_else(|| Error::Other("expected <body> element".to_string()))?;
        document
          .dom_mut()
          .set_attribute(body, "data-external", "1")
          .map_err(|e| Error::Other(e.to_string()))?;
      }
      _ => {}
    }
    self
      .log
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(code);
    Ok(())
  }

  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &fastrender::js::ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut fastrender::BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self.execute_classic_script(script_text, spec, current_script, document, event_loop)
  }
}

#[test]
fn browser_tab_navigate_to_url_executes_inline_and_external_scripts_at_parse_time() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let script_url = site.write("external.js", "external-check");
  let html_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <body>
          <div id="before"></div>
          <script>inline-check</script>
          <div id="after-inline"></div>
          <script src="external.js"></script>
          <div id="after-external"></div>
        </body>
      </html>"#,
  );

  let log = Arc::new(Mutex::new(Vec::<String>::new()));
  let executor = NavigateUrlParseTimeExecutor {
    log: Arc::clone(&log),
    expected_external_url: script_url.clone(),
  };
  let options = RenderOptions::new().with_viewport(1, 1);

  let mut tab = BrowserTab::from_html("", options.clone(), ExecutorWithWindow::new(executor))?;
  tab.navigate_to_url(&html_url, options)?;
  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  assert!(
    find_element_by_id(tab.dom(), "after-inline").is_some(),
    "expected parsing to resume after executing the inline script"
  );
  assert!(
    find_element_by_id(tab.dom(), "after-external").is_some(),
    "expected parsing to resume after executing the external script"
  );

  let body = tab.dom().body().expect("body element after navigation");
  assert_eq!(
    tab
      .dom()
      .get_attribute(body, "data-inline")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("1")
  );
  assert_eq!(
    tab
      .dom()
      .get_attribute(body, "data-external")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("1")
  );

  assert_eq!(
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_slice(),
    &["inline-check".to_string(), "external-check".to_string()]
  );
  Ok(())
}

#[test]
fn browser_tab_lifecycle_events_invoke_js_listeners_and_microtasks() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <script id="setup">
          (function () {
            function push(msg) {
              const el = document.documentElement;
              const prev = el.getAttribute("data-log") || "";
              el.setAttribute("data-log", prev + msg + "|");
            }

            document.addEventListener("readystatechange", () => {
              push("listener:rs:" + document.readyState);
              queueMicrotask(() => push("microtask:rs:" + document.readyState));
            });

            document.addEventListener("DOMContentLoaded", () => {
              push("listener:dom");
              queueMicrotask(() => {
                push("microtask:dom");
                const box = document.getElementById("box");
                if (box) box.setAttribute("data-dom", "1");
              });
            });

            window.addEventListener("load", () => {
              push("listener:load");
              queueMicrotask(() => {
                push("microtask:load");
                const box = document.getElementById("box");
                if (box) box.setAttribute("data-load", "1");
              });
            });
          })();
        </script>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>"#;

  let options = RenderOptions::new().with_viewport(1, 1);
  let mut tab = BrowserTab::from_html(html, options, VmJsBrowserTabExecutor::new())?;
  let outcome = tab.run_until_stable_with_run_limits(RunLimits::unbounded(), 8)?;
  assert!(
    matches!(outcome, fastrender::RunUntilStableOutcome::Stable { .. }),
    "expected run_until_stable to reach Stable, got {outcome:?}"
  );

  let dom = tab.dom();
  let box_id = find_element_by_id(dom, "box").expect("#box id");
  let doc_el = dom.document_element().expect("documentElement");
  let log = dom
    .get_attribute(doc_el, "data-log")
    .map_err(|e| Error::Other(e.to_string()))?;
  let ready_state = dom.ready_state().as_str();
  assert_eq!(
    dom
      .get_attribute(box_id, "data-dom")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("1"),
    "expected DOMContentLoaded listener microtask to mutate DOM (readyState={ready_state}, log={log:?})"
  );
  assert_eq!(
    dom
      .get_attribute(box_id, "data-load")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("1"),
    "expected load listener microtask to mutate DOM (readyState={ready_state}, log={log:?})"
  );

  assert_eq!(
    log,
    Some("listener:rs:interactive|microtask:rs:interactive|listener:dom|microtask:dom|listener:rs:complete|listener:load|microtask:rs:complete|microtask:load|"),
    "expected lifecycle event + microtask ordering to match HTML semantics"
  );

  Ok(())
}

#[test]
fn browser_tab_vm_js_script_mutation_changes_pixels_after_event_loop() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .a { background: rgb(255, 0, 0); }
          .b { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="a"></div>
        <script>
          setTimeout(function() {
            document.getElementById("box").setAttribute("class", "b");
          }, 0);
        </script>
      </body>
    </html>"#;
  let options = RenderOptions::new().with_viewport(64, 64);
  let mut tab = BrowserTab::from_html(html, options, VmJsBrowserTabExecutor::default())?;

  let frame_a = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_a, 32, 32), [255, 0, 0, 255]);

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );

  let frame_b = tab.render_frame()?;
  assert_eq!(rgba_at(&frame_b, 32, 32), [0, 0, 255, 255]);
  Ok(())
}

#[test]
fn browser_tab_vm_js_promise_jobs_run_in_microtask_checkpoint() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <body>
        <div id="box"></div>
        <script>
          document.documentElement.setAttribute("data-x", "0");
          Promise.resolve().then(function() {
            document.documentElement.setAttribute("data-x", "1");
          });
        </script>
      </body>
    </html>"#;
  let options = RenderOptions::new()
    .with_viewport(1, 1);
  let tab = BrowserTab::from_html(html, options, VmJsBrowserTabExecutor::default())?;

  // Parser-inserted scripts run at parse time; by the time the constructor returns the microtask
  // checkpoint after script execution should have drained Promise jobs.
  let html_id = tab
    .dom()
    .document_element()
    .ok_or_else(|| Error::Other("expected documentElement".to_string()))?;
  assert_eq!(
    tab.dom()
      .get_attribute(html_id, "data-x")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("1")
  );
  Ok(())
}

#[test]
fn browser_tab_vm_js_set_timeout_fires_after_virtual_clock_advance() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <body>
        <div id="box"></div>
        <script>
          document.getElementById("box").setAttribute("data-phase", "start");
          setTimeout(function() {
            document.getElementById("box").setAttribute("data-phase", "timer");
          }, 10);
        </script>
      </body>
    </html>"#;
  let options = RenderOptions::new().with_viewport(1, 1);

  let clock = Arc::new(VirtualClock::new());
  let clock_for_loop: Arc<dyn Clock> = clock.clone();
  let event_loop = EventLoop::<BrowserTabHost>::with_clock(clock_for_loop);

  let mut tab = BrowserTab::from_html_with_event_loop(
    html,
    options,
    VmJsBrowserTabExecutor::default(),
    event_loop,
  )?;

  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  let box_id = find_element_by_id(tab.dom(), "box").expect("#box id");
  assert_eq!(
    tab.dom()
      .get_attribute(box_id, "data-phase")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("start"),
    "expected timer callback not to have run before clock advance"
  );

  clock.advance(Duration::from_millis(10));
  assert_eq!(
    tab.run_event_loop_until_idle(RunLimits::unbounded())?,
    RunUntilIdleOutcome::Idle
  );
  assert_eq!(
    tab.dom()
      .get_attribute(box_id, "data-phase")
      .map_err(|e| Error::Other(e.to_string()))?,
    Some("timer")
  );
  Ok(())
}
