use crate::error::Result;
use crate::js::{
  CurrentScriptHost, CurrentScriptStateHandle, EventLoop, RunLimits, RunUntilIdleOutcome,
  RunUntilIdleStopReason, ScriptExecutionLog, ScriptOrchestrator, JsExecutionOptions, RealClock,
};
use crate::js::webidl::VmJsRuntime;
use std::sync::Arc;

use super::BrowserDocumentDom2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunUntilStableStopReason {
  /// Rendering did not converge before exhausting the frame budget.
  MaxFrames { limit: usize },
  /// The underlying JS event loop stopped due to its run limits.
  EventLoop(RunUntilIdleStopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunUntilStableOutcome {
  /// No more runnable tasks/microtasks exist and no render invalidation remains.
  Stable { frames_rendered: usize },
  /// The driver stopped before reaching stability.
  Stopped {
    reason: RunUntilStableStopReason,
    frames_rendered: usize,
  },
}

/// JS-enabled, multi-frame document runtime.
///
/// This is a small host container that couples:
/// - a live `dom2` document + render caching ([`BrowserDocumentDom2`]),
/// - a JS realm/runtime instance (currently [`VmJsRuntime`] for Web IDL scaffolding),
/// - an HTML-ish task/microtask event loop ([`EventLoop`]),
/// - `Document.currentScript` bookkeeping ([`CurrentScriptStateHandle`]/[`ScriptOrchestrator`]).
///
/// The primary API is [`BrowserDocumentJs::run_until_stable`], which drives the JS event loop and
/// conditionally re-renders when DOM mutations invalidate layout/paint.
pub struct BrowserDocumentJs {
  document: BrowserDocumentDom2,
  js_runtime: VmJsRuntime,
  event_loop: Option<EventLoop<BrowserDocumentJs>>,
  script_orchestrator: ScriptOrchestrator,
  current_script_state: CurrentScriptStateHandle,
  js_execution_options: JsExecutionOptions,
  script_execution_log: Option<ScriptExecutionLog>,
}

impl BrowserDocumentJs {
  pub fn new(document: BrowserDocumentDom2) -> Self {
    Self::with_js_execution_options(document, JsExecutionOptions::default())
  }

  pub fn with_js_execution_options(document: BrowserDocumentDom2, js_execution_options: JsExecutionOptions) -> Self {
    let event_loop = EventLoop::with_clock_and_queue_limits(
      Arc::new(RealClock::default()),
      js_execution_options.event_loop_queue_limits,
    );
    Self::with_event_loop_and_js_execution_options(document, event_loop, js_execution_options)
  }

  pub fn with_event_loop(document: BrowserDocumentDom2, event_loop: EventLoop<Self>) -> Self {
    Self::with_event_loop_and_js_execution_options(document, event_loop, JsExecutionOptions::default())
  }

  pub fn with_event_loop_and_js_execution_options(
    document: BrowserDocumentDom2,
    mut event_loop: EventLoop<Self>,
    js_execution_options: JsExecutionOptions,
  ) -> Self {
    // Ensure the provided event loop inherits the queue limits from our config surface.
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);
    Self {
      document,
      js_runtime: VmJsRuntime::new(),
      event_loop: Some(event_loop),
      script_orchestrator: ScriptOrchestrator::new(),
      current_script_state: CurrentScriptStateHandle::default(),
      js_execution_options,
      script_execution_log: None,
    }
  }

  pub fn document(&self) -> &BrowserDocumentDom2 {
    &self.document
  }

  pub fn document_mut(&mut self) -> &mut BrowserDocumentDom2 {
    &mut self.document
  }

  pub fn js_runtime(&self) -> &VmJsRuntime {
    &self.js_runtime
  }

  pub fn js_runtime_mut(&mut self) -> &mut VmJsRuntime {
    &mut self.js_runtime
  }

  pub fn script_orchestrator_mut(&mut self) -> &mut ScriptOrchestrator {
    &mut self.script_orchestrator
  }

  pub fn js_execution_options(&self) -> JsExecutionOptions {
    self.js_execution_options
  }

  /// Enable a bounded FIFO log of executed scripts for debugging.
  pub fn enable_script_execution_log(&mut self, capacity: usize) {
    self.script_execution_log = Some(ScriptExecutionLog::new(capacity));
  }

  /// Returns the script execution log if enabled.
  pub fn script_execution_log(&self) -> Option<&ScriptExecutionLog> {
    self.script_execution_log.as_ref()
  }

  /// Mutable access to the underlying event loop, intended for seeding initial tasks (e.g. HTML
  /// script execution tasks) before calling [`run_until_stable`].
  ///
  /// Note: inside task callbacks, use the `&mut EventLoop` passed to the callback instead of
  /// attempting to reach into the host's stored event loop.
  pub fn event_loop_mut(&mut self) -> &mut EventLoop<Self> {
    self.event_loop.as_mut().expect(
      "BrowserDocumentJs event loop is unavailable (likely inside run_until_stable); use the EventLoop passed to the task callback",
    )
  }

  /// Drive the JS event loop and rerender until no more work remains or a limit is hit.
  ///
  /// This is intentionally deterministic and bounded:
  /// - JS execution is bounded by [`JsExecutionOptions::event_loop_run_limits`] via
  ///   [`EventLoop::run_until_idle`].
  /// - Rendering is bounded by `max_frames`.
  pub fn run_until_stable(&mut self, max_frames: usize) -> Result<RunUntilStableOutcome> {
    self.run_until_stable_with_run_limits(self.js_execution_options.event_loop_run_limits, max_frames)
  }

  pub fn run_until_stable_with_run_limits(
    &mut self,
    limits: RunLimits,
    max_frames: usize,
  ) -> Result<RunUntilStableOutcome> {
    let mut frames_rendered = 0usize;

    loop {
      let mut event_loop = self
        .event_loop
        .take()
        .expect("BrowserDocumentJs should always have an event loop");

      let outcome = event_loop.run_until_idle(self, limits);
      self.event_loop = Some(event_loop);

      match outcome? {
        RunUntilIdleOutcome::Idle => {}
        RunUntilIdleOutcome::Stopped(reason) => {
          return Ok(RunUntilStableOutcome::Stopped {
            reason: RunUntilStableStopReason::EventLoop(reason),
            frames_rendered,
          });
        }
      }

      if self.document.is_dirty() {
        if frames_rendered >= max_frames {
          return Ok(RunUntilStableOutcome::Stopped {
            reason: RunUntilStableStopReason::MaxFrames { limit: max_frames },
            frames_rendered,
          });
        }
        let _pixmap = self.document.render_frame()?;
        frames_rendered += 1;
        continue;
      }

      return Ok(RunUntilStableOutcome::Stable { frames_rendered });
    }
  }
}

impl CurrentScriptHost for BrowserDocumentJs {
  fn current_script_state(&self) -> &CurrentScriptStateHandle {
    &self.current_script_state
  }

  fn script_execution_log(&self) -> Option<&ScriptExecutionLog> {
    self.script_execution_log.as_ref()
  }

  fn script_execution_log_mut(&mut self) -> Option<&mut ScriptExecutionLog> {
    self.script_execution_log.as_mut()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2::NodeKind;
  use crate::dom2::{Document as Dom2Document, NodeId};
  use crate::js::{
    Clock, CurrentScriptHost, JsExecutionOptions, RunLimits, RunUntilIdleStopReason,
    ScriptBlockExecutor, ScriptExecutionLogEntry, ScriptSourceSnapshot, ScriptType, TaskSource,
  };
  use std::cell::RefCell;
  use std::rc::Rc;
  use std::sync::atomic::{AtomicU64, Ordering};
  use std::sync::Arc;
  use std::time::Duration;

  fn renderer_for_tests() -> super::super::FastRender {
    super::super::FastRender::builder()
      .font_sources(crate::text::font_db::FontConfig::bundled_only())
      .build()
      .expect("renderer")
  }

  fn first_text_node_id(doc: &crate::dom2::Document) -> Option<crate::dom2::NodeId> {
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
      let node = doc.node(id);
      if matches!(node.kind, crate::dom2::NodeKind::Text { .. }) {
        return Some(id);
      }
      for &child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn set_first_text(doc: &mut crate::dom2::Document, new_text: &str) {
    let Some(id) = first_text_node_id(doc) else {
      return;
    };
    let node = doc.node_mut(id);
    let NodeKind::Text { content } = &mut node.kind else {
      return;
    };
    content.clear();
    content.push_str(new_text);
  }

  fn find_script_elements(dom: &Dom2Document) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![dom.root()];
    while let Some(id) = stack.pop() {
      let node = dom.node(id);
      if let NodeKind::Element { tag_name, .. } = &node.kind {
        if tag_name.eq_ignore_ascii_case("script") {
          out.push(id);
        }
      }
      for &child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    out
  }

  #[test]
  fn rerenders_after_dom_mutation_task_and_microtask() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut js_options = JsExecutionOptions::default();
    js_options.event_loop_run_limits = RunLimits::unbounded();
    let mut runtime = BrowserDocumentJs::with_js_execution_options(
      BrowserDocumentDom2::new(
        renderer,
        "<!doctype html><html><body><div>Hello</div></body></html>",
        super::super::RenderOptions::new().with_viewport(32, 32),
      )?,
      js_options,
    );

    runtime.document_mut().render_frame()?;

    let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
    let log_for_task = Rc::clone(&log);

    runtime.event_loop_mut().queue_task(TaskSource::Script, move |host, event_loop| {
      log_for_task.borrow_mut().push("task");
      set_first_text(host.document.dom_mut(), "task");

      let log_for_microtask = Rc::clone(&log_for_task);
      event_loop.queue_microtask(move |host, _event_loop| {
        log_for_microtask.borrow_mut().push("microtask");
        set_first_text(host.document.dom_mut(), "microtask");
        Ok(())
      })?;

      Ok(())
    })?;

    let outcome = runtime.run_until_stable(10)?;
    assert_eq!(outcome, RunUntilStableOutcome::Stable { frames_rendered: 1 });
    assert_eq!(&*log.borrow(), &["task", "microtask"]);
    assert!(!runtime.document().is_dirty());
    Ok(())
  }

  #[test]
  fn stops_on_max_tasks_with_infinite_interval() -> Result<()> {
    let renderer = renderer_for_tests();
    let document = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      super::super::RenderOptions::new().with_viewport(32, 32),
    )?;

    #[derive(Debug, Default)]
    struct TickClock {
      now_nanos: AtomicU64,
    }

    impl Clock for TickClock {
      fn now(&self) -> Duration {
        const STEP_NANOS: u64 = 10_000_000;
        let prev = self.now_nanos.fetch_add(STEP_NANOS, Ordering::Relaxed);
        Duration::from_nanos(prev)
      }
    }

    let clock: Arc<dyn Clock> = Arc::new(TickClock::default());
    let event_loop = EventLoop::<BrowserDocumentJs>::with_clock(clock);

    let mut js_options = JsExecutionOptions::default();
    js_options.event_loop_run_limits = RunLimits {
      max_tasks: 5,
      max_microtasks: 100,
      max_wall_time: None,
    };
    let mut runtime =
      BrowserDocumentJs::with_event_loop_and_js_execution_options(document, event_loop, js_options);
    runtime
      .event_loop_mut()
      .set_interval(Duration::from_millis(0), |_host, _event_loop| Ok(()))?;

    let outcome = runtime.run_until_stable(10)?;

    assert!(
      matches!(
        outcome,
        RunUntilStableOutcome::Stopped {
          reason: RunUntilStableStopReason::EventLoop(RunUntilIdleStopReason::MaxTasks { .. }),
          frames_rendered: 0,
        }
      ),
      "unexpected outcome: {outcome:?}"
    );
    Ok(())
  }

  #[derive(Default)]
  struct LoggingExecutor;

  impl ScriptBlockExecutor<BrowserDocumentJs> for LoggingExecutor {
    fn execute_script(
      &mut self,
      host: &mut BrowserDocumentJs,
      _orchestrator: &mut ScriptOrchestrator,
      _dom: &Dom2Document,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> Result<()> {
      assert!(
        host.current_script().is_some(),
        "expected currentScript to be set while classic script executes"
      );
      Ok(())
    }
  }

  #[test]
  fn records_script_execution_log_on_browser_document_js() -> Result<()> {
    let renderer = renderer_for_tests();
    let mut runtime = BrowserDocumentJs::new(BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body>host</body></html>",
      super::super::RenderOptions::new().with_viewport(32, 32),
    )?);
    runtime.enable_script_execution_log(16);

    let renderer_dom = crate::dom::parse_html(
      "<!doctype html><script src=\"https://example.com/a.js\"></script><script></script>",
    )
    .unwrap();
    let dom = Dom2Document::from_renderer_dom(&renderer_dom);
    let scripts = find_script_elements(&dom);
    assert_eq!(scripts.len(), 2);

    let mut executor = LoggingExecutor::default();
    let mut orchestrator = ScriptOrchestrator::new();
    orchestrator.execute_script_element(
      &mut runtime,
      &dom,
      scripts[0],
      ScriptType::Classic,
      &mut executor,
    )?;
    orchestrator.execute_script_element(
      &mut runtime,
      &dom,
      scripts[1],
      ScriptType::Classic,
      &mut executor,
    )?;

    let log = runtime
      .script_execution_log()
      .expect("script execution log enabled");
    assert_eq!(
      log.entries().iter().cloned().collect::<Vec<_>>(),
      vec![
        ScriptExecutionLogEntry {
          script_id: scripts[0].index(),
          source: ScriptSourceSnapshot::Url {
            url: "https://example.com/a.js".to_string()
          },
          current_script_node_id: Some(scripts[0].index()),
        },
        ScriptExecutionLogEntry {
          script_id: scripts[1].index(),
          source: ScriptSourceSnapshot::Inline,
          current_script_node_id: Some(scripts[1].index()),
        },
      ]
    );

    Ok(())
  }
}
