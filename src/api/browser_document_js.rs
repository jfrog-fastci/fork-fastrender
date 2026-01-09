use crate::error::Result;
use crate::js::{
  CurrentScriptHost, CurrentScriptStateHandle, EventLoop, RunAnimationFrameOutcome, RunLimits,
  RunUntilIdleOutcome, RunUntilIdleStopReason, ScriptExecutionLog, ScriptOrchestrator,
  JsExecutionOptions, RealClock,
};
use crate::js::webidl::VmJsRuntime;
use std::sync::Arc;

use super::{BrowserDocumentDom2, Pixmap, RenderOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunUntilStableStopReason {
  /// The JS + animation frame + render stabilization driver did not converge before exhausting the
  /// frame budget.
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
  pub fn from_html(html: &str, options: RenderOptions) -> Result<Self> {
    Ok(Self::new(BrowserDocumentDom2::from_html(html, options)?))
  }

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

  pub fn render_if_needed(&mut self) -> Result<Option<Pixmap>> {
    self.document.render_if_needed()
  }

  /// Execute at most one task turn (or a standalone microtask checkpoint) and return a freshly
  /// rendered frame when the document becomes dirty.
  pub fn tick_frame(&mut self) -> Result<Option<Pixmap>> {
    let mut event_loop = self
      .event_loop
      .take()
      .expect("BrowserDocumentJs should always have an event loop");

    if event_loop.pending_microtask_count() > 0 {
      event_loop.perform_microtask_checkpoint(self)?;
    } else {
      let _ = event_loop.run_next_task(self)?;
    }
    self.event_loop = Some(event_loop);
    self.render_if_needed()
  }

  pub fn js_runtime(&self) -> &VmJsRuntime {
    &self.js_runtime
  }

  pub fn js_runtime_mut(&mut self) -> &mut VmJsRuntime {
    &mut self.js_runtime
  }

  /// Execute a `<script>` element using the runtime's internal [`ScriptOrchestrator`].
  ///
  /// This is a convenience wrapper that avoids borrow-checker pitfalls from trying to call
  /// `runtime.script_orchestrator_mut().execute_script_element(&mut runtime, ..)`.
  pub fn execute_script_element<Exec>(
    &mut self,
    script: crate::dom2::NodeId,
    script_type: crate::js::ScriptType,
    executor: &mut Exec,
  ) -> Result<()>
  where
    Exec: crate::js::ScriptBlockExecutor<Self>,
  {
    let mut orchestrator = std::mem::take(&mut self.script_orchestrator);
    let result = orchestrator.execute_script_element(self, script, script_type, executor);
    self.script_orchestrator = orchestrator;
    result
  }

  /// Mutable access to the underlying [`ScriptOrchestrator`].
  ///
  /// Prefer [`BrowserDocumentJs::execute_script_element`] when executing scripts, as it avoids
  /// borrow-checker issues with simultaneously borrowing the orchestrator and the host runtime.
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
  ///   [`EventLoop::run_until_idle`] (repeatedly).
  /// - Animation frame turns (`requestAnimationFrame`) and rendering are bounded by `max_frames`.
  pub fn run_until_stable(&mut self, max_frames: usize) -> Result<RunUntilStableOutcome> {
    self.run_until_stable_with_run_limits(self.js_execution_options.event_loop_run_limits, max_frames)
  }

  pub fn run_until_stable_with_run_limits(
    &mut self,
    limits: RunLimits,
    max_frames: usize,
  ) -> Result<RunUntilStableOutcome> {
    let mut frames_rendered = 0usize;
    if !self.document.is_dirty()
      && self
        .event_loop
        .as_ref()
        .is_some_and(|event_loop| event_loop.is_idle() && !event_loop.has_pending_animation_frame_callbacks())
    {
      return Ok(RunUntilStableOutcome::Stable { frames_rendered });
    }
    let mut frames_executed = 0usize;

    loop {
      if frames_executed >= max_frames {
        return Ok(RunUntilStableOutcome::Stopped {
          reason: RunUntilStableStopReason::MaxFrames { limit: max_frames },
          frames_rendered,
        });
      }
      frames_executed += 1;

      let mut event_loop = self
        .event_loop
        .take()
        .expect("BrowserDocumentJs should always have an event loop");

      let outcome = event_loop.run_until_idle(self, limits);
      match outcome? {
        RunUntilIdleOutcome::Idle => {}
        RunUntilIdleOutcome::Stopped(reason) => {
          self.event_loop = Some(event_loop);
          return Ok(RunUntilStableOutcome::Stopped {
            reason: RunUntilStableStopReason::EventLoop(reason),
            frames_rendered,
          });
        }
      }

      let raf_outcome = event_loop.run_animation_frame(self)?;
      if matches!(raf_outcome, RunAnimationFrameOutcome::Ran { .. }) {
        let microtask_limits = RunLimits {
          max_tasks: 0,
          max_microtasks: limits.max_microtasks,
          max_wall_time: limits.max_wall_time,
        };
        match event_loop.run_until_idle(self, microtask_limits)? {
          RunUntilIdleOutcome::Idle => {}
          RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. }) => {}
          RunUntilIdleOutcome::Stopped(reason) => {
            self.event_loop = Some(event_loop);
            return Ok(RunUntilStableOutcome::Stopped {
              reason: RunUntilStableStopReason::EventLoop(reason),
              frames_rendered,
            });
          }
        }
      }

      self.event_loop = Some(event_loop);

      if self.document.is_dirty() {
        let _pixmap = self.document.render_frame()?;
        frames_rendered += 1;
      }

      let Some(event_loop) = self.event_loop.as_ref() else {
        return Ok(RunUntilStableOutcome::Stable { frames_rendered });
      };
      if !self.document.is_dirty()
        && event_loop.is_idle()
        && !event_loop.has_pending_animation_frame_callbacks()
      {
        return Ok(RunUntilStableOutcome::Stable { frames_rendered });
      }
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

impl crate::js::DomHost for BrowserDocumentJs {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&crate::dom2::Document) -> R,
  {
    <BrowserDocumentDom2 as crate::js::DomHost>::with_dom(&self.document, f)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut crate::dom2::Document) -> (R, bool),
  {
    <BrowserDocumentDom2 as crate::js::DomHost>::mutate_dom(&mut self.document, f)
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
  fn tick_frame_rerenders_each_task_turn() -> Result<()> {
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

    runtime.event_loop_mut().queue_task(TaskSource::Script, |host, _event_loop| {
      set_first_text(host.document.dom_mut(), "a");
      Ok(())
    })?;
    runtime.event_loop_mut().queue_task(TaskSource::Script, |host, _event_loop| {
      set_first_text(host.document.dom_mut(), "b");
      Ok(())
    })?;

    assert!(runtime.tick_frame()?.is_some(), "expected render after task 1");
    let id = first_text_node_id(runtime.document().dom()).expect("text node");
    let NodeKind::Text { content } = &runtime.document().dom().node(id).kind else {
      panic!("expected text node");
    };
    assert_eq!(content, "a");

    assert!(runtime.tick_frame()?.is_some(), "expected render after task 2");
    let id = first_text_node_id(runtime.document().dom()).expect("text node");
    let NodeKind::Text { content } = &runtime.document().dom().node(id).kind else {
      panic!("expected text node");
    };
    assert_eq!(content, "b");

    assert!(runtime.tick_frame()?.is_none(), "expected no further work");
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
      "<!doctype html><html><body><script src=\"https://example.com/a.js\"></script><script></script></body></html>",
      super::super::RenderOptions::new().with_viewport(32, 32),
    )?);
    runtime.enable_script_execution_log(16);

    let scripts = find_script_elements(runtime.document().dom());
    assert_eq!(scripts.len(), 2);

    let mut executor = LoggingExecutor::default();
    runtime.execute_script_element(
      scripts[0],
      ScriptType::Classic,
      &mut executor,
    )?;
    runtime.execute_script_element(
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
