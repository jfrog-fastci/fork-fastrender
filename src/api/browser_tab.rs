use crate::dom::HTML_NAMESPACE;
use crate::debug::trace::TraceHandle;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::{Error, Result};
use crate::html::base_url_tracker::BaseUrlTracker;
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
use crate::js::{
  CurrentScriptHost, CurrentScriptStateHandle, DomHost, EventLoop, JsExecutionOptions, RunLimits,
  RunAnimationFrameOutcome, RunUntilIdleOutcome, RunUntilIdleStopReason, ScriptBlockExecutor,
  ScriptElementSpec, ScriptId, ScriptOrchestrator, ScriptScheduler, ScriptSchedulerAction,
  ScriptType, TaskSource,
};
use crate::resource::{FetchDestination, FetchRequest};

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use selectors::context::QuirksMode;

use super::{BrowserDocumentDom2, Pixmap, RenderOptions, RunUntilStableOutcome, RunUntilStableStopReason};

pub trait BrowserTabJsExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()>;
}

#[derive(Debug, Clone)]
struct ScriptEntry {
  node_id: NodeId,
  spec: ScriptElementSpec,
}

/// RAII guard that increments a host-local "JS execution depth" counter.
///
/// HTML gates certain microtask checkpoints based on whether the **JavaScript execution context
/// stack is empty**. Parsing and navigation can run inside event-loop tasks, so the event loop's
/// "currently running task" state is not equivalent to the JS execution context stack.
struct JsExecutionGuard {
  depth: Rc<Cell<usize>>,
}

impl JsExecutionGuard {
  fn enter(depth: &Rc<Cell<usize>>) -> Self {
    depth.set(depth.get().saturating_add(1));
    Self {
      depth: Rc::clone(depth),
    }
  }
}

impl Drop for JsExecutionGuard {
  fn drop(&mut self) {
    let current = self.depth.get();
    self.depth.set(current.saturating_sub(1));
  }
}

pub struct BrowserTabHost {
  trace: TraceHandle,
  document: BrowserDocumentDom2,
  executor: Box<dyn BrowserTabJsExecutor>,
  current_script: CurrentScriptStateHandle,
  orchestrator: ScriptOrchestrator,
  scheduler: ScriptScheduler<NodeId>,
  scripts: HashMap<ScriptId, ScriptEntry>,
  executed: HashSet<ScriptId>,
  parser_blocked_on: Option<ScriptId>,
  document_url: Option<String>,
  external_script_sources: HashMap<String, String>,
  js_execution_options: JsExecutionOptions,
  js_execution_depth: Rc<Cell<usize>>,
}

impl BrowserTabHost {
  fn new(
    document: BrowserDocumentDom2,
    executor: Box<dyn BrowserTabJsExecutor>,
    trace: TraceHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Self {
    Self {
      trace,
      document,
      executor,
      current_script: CurrentScriptStateHandle::default(),
      orchestrator: ScriptOrchestrator::new(),
      scheduler: ScriptScheduler::new(),
      scripts: HashMap::new(),
      executed: HashSet::new(),
      parser_blocked_on: None,
      document_url: None,
      external_script_sources: HashMap::new(),
      js_execution_options,
      js_execution_depth: Rc::new(Cell::new(0)),
    }
  }

  fn register_external_script_source(&mut self, url: String, source: String) {
    self.external_script_sources.insert(url, source);
  }

  pub fn dom(&self) -> &Document {
    self.document.dom()
  }

  pub fn dom_mut(&mut self) -> &mut Document {
    self.document.dom_mut()
  }

  pub fn current_script_node(&self) -> Option<NodeId> {
    self.current_script.borrow().current_script
  }

  fn reset_scripting_state(&mut self, document_url: Option<String>) {
    self.current_script = CurrentScriptStateHandle::default();
    self.orchestrator = ScriptOrchestrator::new();
    self.scheduler = ScriptScheduler::new();
    self.scripts.clear();
    self.executed.clear();
    self.parser_blocked_on = None;
    self.document_url = document_url;
    self.js_execution_depth.set(0);
  }

  fn discover_scripts_best_effort(&self, document_url: Option<&str>) -> Vec<(NodeId, ScriptElementSpec)> {
    fn is_html_namespace(namespace: &str) -> bool {
      namespace.is_empty() || namespace == HTML_NAMESPACE
    }

    let dom = self.document.dom();
    let mut base_url_tracker = BaseUrlTracker::new(document_url);
    let mut out: Vec<(NodeId, ScriptElementSpec)> = Vec::new();

    let mut stack: Vec<(NodeId, bool, bool, bool)> = Vec::new();
    stack.push((dom.root(), false, false, false));

    while let Some((id, in_head, in_foreign_namespace, in_template)) = stack.pop() {
      let node = dom.node(id);

      // Shadow roots are treated as separate trees for script discovery/execution.
      if matches!(node.kind, NodeKind::ShadowRoot { .. }) {
        continue;
      }

      // HTML: "prepare a script" early-outs when the script element is not connected. Be robust
      // against partially-detached nodes that may still appear in a parent's `children` list.
      if !dom.is_connected_for_scripting(id) {
        continue;
      }

      let mut next_in_head = in_head;
      let mut next_in_template = in_template;
      let mut next_in_foreign_namespace = in_foreign_namespace;

      match &node.kind {
        NodeKind::Element {
          tag_name,
          namespace,
          attributes,
        } => {
          base_url_tracker.on_element_inserted(
            tag_name,
            namespace,
            attributes,
            in_head,
            in_foreign_namespace,
            in_template,
          );

          if tag_name.eq_ignore_ascii_case("script") && is_html_namespace(namespace) {
            let base_url = base_url_tracker.current_base_url();
            let async_attr = dom.has_attribute(id, "async").unwrap_or(false);
            let defer_attr = dom.has_attribute(id, "defer").unwrap_or(false);
            let src_attr_present = dom.has_attribute(id, "src").unwrap_or(false);
            let src = dom
              .get_attribute(id, "src")
              .ok()
              .flatten()
              .and_then(|value| base_url_tracker.resolve_script_src(value));

            let mut inline_text = String::new();
            for &child in &node.children {
              if let NodeKind::Text { content } = &dom.node(child).kind {
                inline_text.push_str(content);
              }
            }

            // Reuse the shared HTML script `type`/`language` classification logic.
            let script_type = crate::js::determine_script_type_dom2(dom, id);

            out.push((
              id,
              ScriptElementSpec {
                base_url,
                src,
                src_attr_present,
                inline_text,
                async_attr,
                defer_attr,
                parser_inserted: true,
                node_id: Some(id),
                script_type,
              },
            ));
          }

          let is_head = tag_name.eq_ignore_ascii_case("head") && is_html_namespace(namespace);
          next_in_head = in_head || is_head;
          let is_template = tag_name.eq_ignore_ascii_case("template") && is_html_namespace(namespace);
          next_in_template = in_template || is_template;
          next_in_foreign_namespace = in_foreign_namespace || !is_html_namespace(namespace);
        }
        NodeKind::Slot {
          namespace,
          attributes,
          ..
        } => {
          base_url_tracker.on_element_inserted(
            "slot",
            namespace,
            attributes,
            in_head,
            in_foreign_namespace,
            in_template,
          );
          next_in_foreign_namespace = in_foreign_namespace || !is_html_namespace(namespace);
        }
        _ => {}
      }

      // Inert subtrees (template contents) should not be traversed for script execution.
      if node.inert_subtree {
        continue;
      }

      // Push children in reverse so we traverse left-to-right in document order.
      for &child in node.children.iter().rev() {
        stack.push((child, next_in_head, next_in_foreign_namespace, next_in_template));
      }
    }

    out
  }

  fn register_and_schedule_script(
    &mut self,
    node_id: NodeId,
    spec: ScriptElementSpec,
    base_url_at_discovery: Option<String>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<ScriptId> {
    // HTML `</script>` handling performs a microtask checkpoint *before* preparing the script, but
    // only when the JS execution context stack is empty.
    if spec.parser_inserted && self.js_execution_depth.get() == 0 {
      event_loop.perform_microtask_checkpoint(self)?;
    }

    let spec_for_table = spec.clone();
    if spec_for_table.script_type == ScriptType::Classic && !spec_for_table.src_attr_present {
      self
        .js_execution_options
        .check_script_source(&spec_for_table.inline_text, "source=inline")?;
    }
    let discovered = self
      .scheduler
      .discovered_parser_script(spec, node_id, base_url_at_discovery)?;
    self.scripts.insert(
      discovered.id,
      ScriptEntry {
        node_id,
        spec: spec_for_table,
      },
    );
    self.apply_scheduler_actions(discovered.actions, event_loop)?;
    Ok(discovered.id)
  }

  fn apply_scheduler_actions(
    &mut self,
    actions: Vec<ScriptSchedulerAction<NodeId>>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    for action in actions {
      match action {
        ScriptSchedulerAction::StartFetch { script_id, url, .. } => {
          self.start_fetch(script_id, url, event_loop)?;
        }
        ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
          if self.executed.contains(&script_id) {
            continue;
          }
          if self
            .parser_blocked_on
            .is_some_and(|existing| existing != script_id)
          {
            return Err(Error::Other(
              "ScriptScheduler requested multiple simultaneous parser blocks".to_string(),
            ));
          }
          self.parser_blocked_on = Some(script_id);
        }
        ScriptSchedulerAction::ExecuteNow {
          script_id,
          source_text,
          ..
        } => {
          let result = {
            let _guard = JsExecutionGuard::enter(&self.js_execution_depth);
            let result = self.execute_script(script_id, &source_text, event_loop);
            // Ensure a script failure doesn't leave parsing blocked forever.
            self.finish_script_execution(script_id);
            result
          };
          result?;

          // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
          // execution context stack is empty. Nested (re-entrant) script execution must not drain
          // microtasks until the outermost script returns.
          if self.js_execution_depth.get() == 0 {
            event_loop.perform_microtask_checkpoint(self)?;
          }
        }
        ScriptSchedulerAction::QueueTask {
          script_id,
          source_text,
          ..
        } => {
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            let _guard = JsExecutionGuard::enter(&host.js_execution_depth);
            let result = host.execute_script(script_id, &source_text, event_loop);
            host.finish_script_execution(script_id);
            result
          })?;
        }
      }
    }
    Ok(())
  }

  fn finish_script_execution(&mut self, script_id: ScriptId) {
    self.executed.insert(script_id);
    if self.parser_blocked_on == Some(script_id) {
      self.parser_blocked_on = None;
    }
  }

  fn execute_script(
    &mut self,
    script_id: ScriptId,
    source_text: &str,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    if self.executed.contains(&script_id) {
      return Ok(());
    }

    let Some(entry) = self.scripts.get(&script_id).cloned() else {
      return Err(Error::Other(format!(
        "ScriptScheduler requested execution for unknown script_id={}",
        script_id.as_u64()
      )));
    };

    let node_id = entry.node_id;
    let script_type = entry.spec.script_type;

    struct Adapter<'a> {
      script_id: ScriptId,
      source_text: &'a str,
      spec: &'a ScriptElementSpec,
      event_loop: &'a mut EventLoop<BrowserTabHost>,
    }

    impl ScriptBlockExecutor<BrowserTabHost> for Adapter<'_> {
      fn execute_script(
        &mut self,
        host: &mut BrowserTabHost,
        _orchestrator: &mut ScriptOrchestrator,
        _script: NodeId,
        script_type: ScriptType,
      ) -> Result<()> {
        if script_type != ScriptType::Classic {
          return Ok(());
        }

        let mut span = host.trace.span("js.script.execute", "js");
        span.arg_u64("script_id", self.script_id.as_u64());
        if let Some(url) = self.spec.src.as_deref() {
          span.arg_str("url", url);
        }
        span.arg_bool("async_attr", self.spec.async_attr);
        span.arg_bool("defer_attr", self.spec.defer_attr);
        span.arg_bool("parser_inserted", self.spec.parser_inserted);

        let current_script = host.current_script_node();
        let (document, executor) = (&mut host.document, &mut host.executor);
        executor.execute_classic_script(
          self.source_text,
          self.spec,
          current_script,
          document,
          self.event_loop,
        )
      }
    }

    let mut adapter = Adapter {
      script_id,
      source_text,
      spec: &entry.spec,
      event_loop,
    };

    // Avoid double-borrowing `self` by temporarily moving the orchestrator out.
    let mut orchestrator = std::mem::take(&mut self.orchestrator);
    let result = orchestrator.execute_script_element(self, node_id, script_type, &mut adapter);
    self.orchestrator = orchestrator;
    result
  }

  fn start_fetch(
    &mut self,
    script_id: ScriptId,
    url: String,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let is_blocking = self
      .scripts
      .get(&script_id)
      .is_some_and(|entry| entry.spec.src_attr_present && !entry.spec.async_attr && !entry.spec.defer_attr);

    if is_blocking {
      let source = self.fetch_script_source(script_id, &url)?;
      let actions = self.scheduler.fetch_completed(script_id, source)?;
      self.apply_scheduler_actions(actions, event_loop)?;
      return Ok(());
    }

    event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      let source = host.fetch_script_source(script_id, &url)?;
      let actions = host.scheduler.fetch_completed(script_id, source)?;
      host.apply_scheduler_actions(actions, event_loop)?;
      Ok(())
    })?;
    Ok(())
  }

  fn fetch_script_source(&self, script_id: ScriptId, url: &str) -> Result<String> {
    let mut span = self.trace.span("js.script.fetch", "js");
    span.arg_u64("script_id", script_id.as_u64());
    span.arg_str("url", url);

    if let Some(source) = self.external_script_sources.get(url) {
      span.arg_u64("bytes", source.as_bytes().len() as u64);
      self.js_execution_options.check_script_source_bytes(
        source.as_bytes().len(),
        &format!("source=external url={url}"),
      )?;
      return Ok(source.clone());
    }

    let fetcher = self.document.fetcher();
    let mut req = FetchRequest::new(url, FetchDestination::Other);
    if let Some(referrer) = self.document_url.as_deref() {
      req = req.with_referrer_url(referrer);
    }
    let resource = fetcher.fetch_with_request(req)?;
    span.arg_u64("bytes", resource.bytes.len() as u64);
    self.js_execution_options.check_script_source_bytes(
      resource.bytes.len(),
      &format!("source=external url={url}"),
    )?;
    Ok(String::from_utf8_lossy(&resource.bytes).to_string())
  }
}

impl CurrentScriptHost for BrowserTabHost {
  fn current_script_state(&self) -> &CurrentScriptStateHandle {
    &self.current_script
  }
}

impl DomHost for BrowserTabHost {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Document) -> R,
  {
    <BrowserDocumentDom2 as DomHost>::with_dom(&self.document, f)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Document) -> (R, bool),
  {
    <BrowserDocumentDom2 as DomHost>::mutate_dom(&mut self.document, f)
  }
}

pub struct BrowserTab {
  trace: TraceHandle,
  trace_output: Option<PathBuf>,
  host: BrowserTabHost,
  event_loop: EventLoop<BrowserTabHost>,
}

impl BrowserTab {
  fn parse_html_streaming_and_schedule_scripts(
    &mut self,
    html: &str,
    document_url: Option<&str>,
  ) -> Result<()> {
    let mut parser = StreamingHtmlParser::new(document_url);
    parser.push_str(html);
    parser.set_eof();

    loop {
      match parser.pump() {
        StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        } => {
          let snapshot = {
            let Some(doc) = parser.document() else {
              return Err(Error::Other(
                "StreamingHtmlParser yielded a script without an active document".to_string(),
              ));
            };
            doc.clone_with_events()
          };

          self.host.mutate_dom(|dom| {
            *dom = snapshot;
            ((), true)
          });

          // HTML: before executing a parser-inserted script at a script end-tag boundary, perform a
          // microtask checkpoint when the JS execution context stack is empty. For this integration
          // point, approximate that by checking whether the event loop is currently executing a
          // task.
          if self.event_loop.currently_running_task().is_none() {
            self.event_loop.perform_microtask_checkpoint(&mut self.host)?;
          }

          let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
          let spec =
            crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2(self.host.dom(), script, &base);
          let base_url_at_discovery = spec.base_url.clone();
          self.on_parser_discovered_script(script, spec, base_url_at_discovery)?;

          // Sync any DOM mutations from the executed script back into the streaming parser's live
          // DOM before resuming parsing.
          let updated = self.host.dom().clone_with_events();
          {
            let Some(mut doc) = parser.document_mut() else {
              return Err(Error::Other(
                "StreamingHtmlParser yielded a script without an active document".to_string(),
              ));
            };
            *doc = updated;
          }
        }
        StreamingParserYield::NeedMoreInput => {
          return Err(Error::Other(
            "StreamingHtmlParser unexpectedly requested more input after EOF".to_string(),
          ));
        }
        StreamingParserYield::Finished { document } => {
          self.host.mutate_dom(|dom| {
            *dom = document;
            ((), true)
          });
          self.on_parsing_completed()?;
          return Ok(());
        }
      }
    }
  }

  pub fn from_html<E>(html: &str, options: RenderOptions, executor: E) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    Self::from_html_with_js_execution_options(html, options, executor, JsExecutionOptions::default())
  }

  pub fn from_html_with_event_loop<E>(
    html: &str,
    options: RenderOptions,
    executor: E,
    event_loop: EventLoop<BrowserTabHost>,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    Self::from_html_with_event_loop_and_js_execution_options(
      html,
      options,
      executor,
      event_loop,
      JsExecutionOptions::default(),
    )
  }

  pub fn from_html_with_js_execution_options<E>(
    html: &str,
    options: RenderOptions,
    executor: E,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    let trace_session = super::TraceSession::from_options(Some(&options));
    let trace_handle = trace_session.handle.clone();
    let trace_output = trace_session.output.clone();

    // Parse-time script execution requires a script-aware streaming parser driver. Start the tab
    // with an empty DOM and then stream-parse the provided HTML, pausing at `</script>` boundaries.
    let document = BrowserDocumentDom2::from_html("", options.clone())?;
    let host = BrowserTabHost::new(
      document,
      Box::new(executor),
      trace_handle.clone(),
      js_execution_options,
    );
    let mut event_loop = EventLoop::new();
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      host,
      event_loop,
    };
    tab.host.reset_scripting_state(None);
    tab.parse_html_streaming_and_schedule_scripts(html, None)?;
    Ok(tab)
  }

  pub fn from_html_with_event_loop_and_js_execution_options<E>(
    html: &str,
    options: RenderOptions,
    executor: E,
    mut event_loop: EventLoop<BrowserTabHost>,
    js_execution_options: JsExecutionOptions,
  ) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    let trace_session = super::TraceSession::from_options(Some(&options));
    let trace_handle = trace_session.handle.clone();
    let trace_output = trace_session.output.clone();

    let document = BrowserDocumentDom2::from_html(html, options)?;
    let host = BrowserTabHost::new(
      document,
      Box::new(executor),
      trace_handle.clone(),
      js_execution_options,
    );
    event_loop.set_trace_handle(trace_handle.clone());
    event_loop.set_queue_limits(js_execution_options.event_loop_queue_limits);

    let mut tab = Self {
      trace: trace_handle,
      trace_output,
      host,
      event_loop,
    };
    tab.host.reset_scripting_state(None);
    tab.discover_and_schedule_scripts(None)?;
    Ok(tab)
  }

  pub fn register_script_source(&mut self, url: impl Into<String>, source: impl Into<String>) {
    self
      .host
      .register_external_script_source(url.into(), source.into());
  }

  pub fn write_trace(&self) -> Result<()> {
    let Some(path) = self.trace_output.as_deref() else {
      return Ok(());
    };
    self.trace.write_chrome_trace(path).map_err(Error::Io)
  }

  pub fn navigate_to_html(&mut self, html: &str, options: RenderOptions) -> Result<()> {
    let trace_session = super::TraceSession::from_options(Some(&options));
    self.trace = trace_session.handle.clone();
    self.trace_output = trace_session.output.clone();

    self
      .host
      .document
      .reset_with_dom(Document::new(QuirksMode::NoQuirks), options);
    self.reset_event_loop();
    self.host.trace = self.trace.clone();
    self.host.reset_scripting_state(None);
    self.parse_html_streaming_and_schedule_scripts(html, None)
  }

  pub fn navigate_to_url(&mut self, url: &str, options: RenderOptions) -> Result<()> {
    let trace_session = super::TraceSession::from_options(Some(&options));
    self.trace = trace_session.handle.clone();
    self.trace_output = trace_session.output.clone();

    let report = self.host.document.navigate_url(url, options)?;
    self.reset_event_loop();
    self.host.trace = self.trace.clone();
    self.host.reset_scripting_state(report.final_url.clone());
    self.discover_and_schedule_scripts(report.final_url.as_deref())
  }

  pub fn run_event_loop_until_idle(&mut self, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    self.event_loop.run_until_idle(&mut self.host, limits)
  }

  pub fn js_execution_options(&self) -> JsExecutionOptions {
    self.host.js_execution_options
  }

  pub fn set_js_execution_options(&mut self, options: JsExecutionOptions) {
    self.host.js_execution_options = options;
    self.event_loop.set_queue_limits(options.event_loop_queue_limits);
  }

  pub fn run_until_stable(&mut self, max_frames: usize) -> Result<RunUntilStableOutcome> {
    self.run_until_stable_with_run_limits(self.host.js_execution_options.event_loop_run_limits, max_frames)
  }

  pub fn run_until_stable_with_run_limits(
    &mut self,
    limits: RunLimits,
    max_frames: usize,
  ) -> Result<RunUntilStableOutcome> {
    let mut frames_rendered = 0usize;
    if !self.host.document.is_dirty()
      && self.event_loop.is_idle()
      && !self.event_loop.has_pending_animation_frame_callbacks()
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

      match self.run_event_loop_until_idle(limits)? {
        RunUntilIdleOutcome::Idle => {}
        RunUntilIdleOutcome::Stopped(reason) => {
          return Ok(RunUntilStableOutcome::Stopped {
            reason: RunUntilStableStopReason::EventLoop(reason),
            frames_rendered,
          });
        }
      }

      let raf_outcome = self.event_loop.run_animation_frame(&mut self.host)?;
      if matches!(raf_outcome, RunAnimationFrameOutcome::Ran { .. }) {
        // HTML: microtask checkpoint after rAF callbacks.
        //
        // We run this as a "microtasks only" spin so that:
        // - microtasks queued by rAF are drained immediately,
        // - normal tasks (including timer tasks) are not run until the next loop iteration (after
        //   rendering).
        let microtask_limits = RunLimits {
          max_tasks: 0,
          max_microtasks: limits.max_microtasks,
          max_wall_time: limits.max_wall_time,
        };
        match self.event_loop.run_until_idle(&mut self.host, microtask_limits)? {
          RunUntilIdleOutcome::Idle => {}
          RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. }) => {
            // Expected: tasks are present, but this checkpoint only drains microtasks.
          }
          RunUntilIdleOutcome::Stopped(reason) => {
            return Ok(RunUntilStableOutcome::Stopped {
              reason: RunUntilStableStopReason::EventLoop(reason),
              frames_rendered,
            });
          }
        }
      }

      if self.host.document.is_dirty() {
        let _pixmap = self.host.document.render_frame()?;
        frames_rendered += 1;
      }

      if !self.host.document.is_dirty()
        && self.event_loop.is_idle()
        && !self.event_loop.has_pending_animation_frame_callbacks()
      {
        return Ok(RunUntilStableOutcome::Stable { frames_rendered });
      }
    }
  }

  /// Execute at most one task turn (or a standalone microtask checkpoint) and return a freshly
  /// rendered frame when the document becomes dirty.
  pub fn tick_frame(&mut self) -> Result<Option<Pixmap>> {
    let run_limits = self.host.js_execution_options.event_loop_run_limits;
    if self.event_loop.pending_microtask_count() > 0 {
      // Drain microtasks only (HTML microtask checkpoint), but do not run any tasks.
      let microtask_limits = RunLimits {
        max_tasks: 0,
        max_microtasks: run_limits.max_microtasks,
        max_wall_time: run_limits.max_wall_time,
      };
      match self.event_loop.run_until_idle(&mut self.host, microtask_limits)? {
        RunUntilIdleOutcome::Idle
        | RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. }) => {}
        RunUntilIdleOutcome::Stopped(reason) => {
          return Err(Error::Other(format!(
            "BrowserTab::tick_frame microtask checkpoint stopped: {reason:?}"
          )))
        }
      }
    } else {
      // Run exactly one task turn (a task + its post-task microtask checkpoint).
      let one_task_limits = RunLimits {
        max_tasks: 1,
        max_microtasks: run_limits.max_microtasks,
        max_wall_time: run_limits.max_wall_time,
      };
      match self.event_loop.run_until_idle(&mut self.host, one_task_limits)? {
        RunUntilIdleOutcome::Idle
        | RunUntilIdleOutcome::Stopped(RunUntilIdleStopReason::MaxTasks { .. }) => {}
        RunUntilIdleOutcome::Stopped(reason) => {
          return Err(Error::Other(format!(
            "BrowserTab::tick_frame task turn stopped: {reason:?}"
          )))
        }
      }
    }
    self.render_if_needed()
  }

  pub fn render_if_needed(&mut self) -> Result<Option<Pixmap>> {
    self.host.document.render_if_needed()
  }

  pub fn render_frame(&mut self) -> Result<Pixmap> {
    self.host.document.render_frame()
  }

  pub fn dom(&self) -> &Document {
    self.host.dom()
  }

  pub fn dom_mut(&mut self) -> &mut Document {
    self.host.dom_mut()
  }

  fn reset_event_loop(&mut self) {
    let mut event_loop = EventLoop::new();
    event_loop.set_trace_handle(self.trace.clone());
    event_loop.set_queue_limits(self.host.js_execution_options.event_loop_queue_limits);
    self.event_loop = event_loop;
  }

  /// Notify the tab that the HTML parser discovered a parser-inserted `<script>` element.
  ///
  /// This is the integration point used by the script-aware streaming HTML parser driver
  /// (`StreamingHtmlParser`). `navigate_to_url` still performs best-effort post-parse discovery for
  /// now.
  pub(crate) fn on_parser_discovered_script(
    &mut self,
    node_id: NodeId,
    spec: ScriptElementSpec,
    base_url_at_discovery: Option<String>,
  ) -> Result<ScriptId> {
    self.host.register_and_schedule_script(
      node_id,
      spec,
      base_url_at_discovery,
      &mut self.event_loop,
    )
  }

  /// Notify the tab that HTML parsing completed.
  ///
  /// This allows deferred scripts to be queued once parsing reaches EOF.
  pub(crate) fn on_parsing_completed(&mut self) -> Result<()> {
    let actions = self.host.scheduler.parsing_completed()?;
    self.host.apply_scheduler_actions(actions, &mut self.event_loop)?;
    Ok(())
  }

  fn discover_and_schedule_scripts(&mut self, document_url: Option<&str>) -> Result<()> {
    let discovered = self.host.discover_scripts_best_effort(document_url);
    for (node_id, spec) in discovered {
      let base_url_at_discovery = spec.base_url.clone();
      self
        .host
        .register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut self.event_loop)?;
    }

    self.on_parsing_completed()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::cell::RefCell;
  use std::rc::Rc;

  struct TestExecutor {
    log: Rc<RefCell<Vec<String>>>,
  }

  impl BrowserTabJsExecutor for TestExecutor {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _current_script: Option<NodeId>,
      _document: &mut BrowserDocumentDom2,
      event_loop: &mut EventLoop<BrowserTabHost>,
    ) -> Result<()> {
      self
        .log
        .borrow_mut()
        .push(format!("script:{script_text}"));
      let log = Rc::clone(&self.log);
      let name = script_text.to_string();
      event_loop.queue_microtask(move |_host, _event_loop| {
        log.borrow_mut().push(format!("microtask:{name}"));
        Ok(())
      })?;
      Ok(())
    }
  }

  fn build_host(html: &str, log: Rc<RefCell<Vec<String>>>) -> Result<(BrowserTabHost, EventLoop<BrowserTabHost>)> {
    let document = BrowserDocumentDom2::from_html(html, RenderOptions::default())?;
    let mut host = BrowserTabHost::new(
      document,
      Box::new(TestExecutor { log }),
      TraceHandle::default(),
      JsExecutionOptions::default(),
    );
    host.reset_scripting_state(None);
    Ok((host, EventLoop::new()))
  }

  #[test]
  fn microtasks_run_at_parser_script_boundaries_when_js_stack_empty() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host("<script>A</script>", Rc::clone(&log))?;

    event_loop.queue_microtask({
      let log = Rc::clone(&log);
      move |_host, _event_loop| {
        log.borrow_mut().push("pre".to_string());
        Ok(())
      }
    })?;

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(
      &*log.borrow(),
      &[
        "pre".to_string(),
        "script:A".to_string(),
        "microtask:A".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn microtask_checkpoint_after_execute_now_is_gated_on_js_execution_depth() -> Result<()> {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let (mut host, mut event_loop) = build_host("<script>B</script>", Rc::clone(&log))?;

    let _outer_guard = JsExecutionGuard::enter(&host.js_execution_depth);

    let mut discovered = host.discover_scripts_best_effort(None);
    assert_eq!(discovered.len(), 1);
    let (node_id, spec) = discovered.pop().unwrap();
    let base_url_at_discovery = spec.base_url.clone();

    host.register_and_schedule_script(node_id, spec, base_url_at_discovery, &mut event_loop)?;

    assert_eq!(&*log.borrow(), &["script:B".to_string()]);
    assert_eq!(event_loop.pending_microtask_count(), 1);

    drop(_outer_guard);
    event_loop.perform_microtask_checkpoint(&mut host)?;

    assert_eq!(
      &*log.borrow(),
      &["script:B".to_string(), "microtask:B".to_string()]
    );
    Ok(())
  }
}
