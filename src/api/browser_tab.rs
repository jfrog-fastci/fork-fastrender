use crate::dom::HTML_NAMESPACE;
use crate::debug::trace::TraceHandle;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::{Error, Result};
use crate::html::base_url_tracker::BaseUrlTracker;
use crate::js::{
  CurrentScriptHost, CurrentScriptStateHandle, DomHost, EventLoop, RunLimits, RunUntilIdleOutcome,
  ScriptBlockExecutor, ScriptElementSpec, ScriptId, ScriptOrchestrator, ScriptScheduler,
  ScriptSchedulerAction, ScriptType, TaskSource,
};
use crate::resource::{FetchDestination, FetchRequest};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::{BrowserDocumentDom2, Pixmap, RenderOptions};

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
}

impl BrowserTabHost {
  fn new(
    document: BrowserDocumentDom2,
    executor: Box<dyn BrowserTabJsExecutor>,
    trace: TraceHandle,
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

            // Reuse the JS `type`/`language` classification logic by building a minimal renderer-dom
            // node. This keeps behavior consistent with other JS plumbing without requiring a
            // separate dom2-specific implementation.
            let script_type = crate::js::determine_script_type(&crate::dom::DomNode {
              node_type: crate::dom::DomNodeType::Element {
                tag_name: tag_name.to_string(),
                namespace: namespace.to_string(),
                attributes: attributes.clone(),
              },
              children: Vec::new(),
            });

            out.push((
              id,
              ScriptElementSpec {
                base_url,
                src,
                inline_text,
                async_attr,
                defer_attr,
                parser_inserted: true,
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
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let base_url_at_discovery = spec.base_url.clone();
    let spec_for_table = spec.clone();
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
    Ok(())
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
          let result = self.execute_script(script_id, &source_text, event_loop);
          // Ensure a script failure doesn't leave parsing blocked forever.
          self.finish_script_execution(script_id);
          result?;
          event_loop.perform_microtask_checkpoint(self)?;
        }
        ScriptSchedulerAction::QueueTask {
          script_id,
          source_text,
          ..
        } => {
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
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
      .is_some_and(|entry| entry.spec.src.is_some() && !entry.spec.async_attr && !entry.spec.defer_attr);

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
      return Ok(source.clone());
    }

    let fetcher = self.document.fetcher();
    let mut req = FetchRequest::new(url, FetchDestination::Other);
    if let Some(referrer) = self.document_url.as_deref() {
      req = req.with_referrer_url(referrer);
    }
    let resource = fetcher.fetch_with_request(req)?;
    span.arg_u64("bytes", resource.bytes.len() as u64);
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
  pub fn from_html<E>(html: &str, options: RenderOptions, executor: E) -> Result<Self>
  where
    E: BrowserTabJsExecutor + 'static,
  {
    let trace_session = super::TraceSession::from_options(Some(&options));
    let trace_handle = trace_session.handle.clone();
    let trace_output = trace_session.output.clone();

    let document = BrowserDocumentDom2::from_html(html, options)?;
    let host = BrowserTabHost::new(document, Box::new(executor), trace_handle.clone());
    let mut event_loop = EventLoop::new();
    event_loop.set_trace_handle(trace_handle.clone());

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

    self.host.document.reset_with_html(html, options)?;
    self.event_loop = EventLoop::new();
    self.event_loop.set_trace_handle(self.trace.clone());
    self.host.trace = self.trace.clone();
    self.host.reset_scripting_state(None);
    self.discover_and_schedule_scripts(None)
  }

  pub fn navigate_to_url(&mut self, url: &str, options: RenderOptions) -> Result<()> {
    let trace_session = super::TraceSession::from_options(Some(&options));
    self.trace = trace_session.handle.clone();
    self.trace_output = trace_session.output.clone();

    let report = self.host.document.navigate_url(url, options)?;
    self.event_loop = EventLoop::new();
    self.event_loop.set_trace_handle(self.trace.clone());
    self.host.trace = self.trace.clone();
    self.host.reset_scripting_state(report.final_url.clone());
    self.discover_and_schedule_scripts(report.final_url.as_deref())
  }

  pub fn run_event_loop_until_idle(&mut self, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    self.event_loop.run_until_idle(&mut self.host, limits)
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

  fn discover_and_schedule_scripts(&mut self, document_url: Option<&str>) -> Result<()> {
    let discovered = self.host.discover_scripts_best_effort(document_url);
    for (node_id, spec) in discovered {
      self
        .host
        .register_and_schedule_script(node_id, spec, &mut self.event_loop)?;
    }

    let actions = self.host.scheduler.parsing_completed()?;
    self.host.apply_scheduler_actions(actions, &mut self.event_loop)?;
    Ok(())
  }
}
