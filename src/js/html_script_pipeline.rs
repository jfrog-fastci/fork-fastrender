//! Streaming HTML parser + HTML script scheduler + event loop integration harness.
//!
//! This is an end-to-end, deterministic driver that integrates:
//! - [`crate::html::streaming_parser::StreamingHtmlParser`] pause points,
//! - [`super::html_script_scheduler::HtmlScriptScheduler`],
//! - [`super::event_loop::EventLoop`],
//! - and [`super::orchestrator::ScriptOrchestrator`] for `Document.currentScript` bookkeeping.
//!
//! It is intentionally separate from [`super::streaming_pipeline`] (classic-only) so we can migrate
//! the engine in stages.

use crate::dom2::{Document, NodeId};
use crate::error::{Error, Result};
use crate::html::base_url_tracker::BaseUrlTracker;
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};

use super::event_loop::{EventLoop, TaskSource};
use super::html_script_scheduler::{
  HtmlScriptId, HtmlScriptScheduler, HtmlScriptSchedulerAction, HtmlScriptWork, ScriptEventKind,
};
use super::orchestrator::{CurrentScriptHost, ScriptBlockExecutor, ScriptOrchestrator};
use super::streaming_dom2::build_parser_inserted_script_element_spec_dom2;
use super::{DomHost, ParseBudget, ScriptType};

use super::runtime::with_event_loop;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

struct JsExecutionGuard {
  depth: Rc<Cell<usize>>,
}

impl JsExecutionGuard {
  fn enter(depth: &Rc<Cell<usize>>) -> Self {
    let cur = depth.get();
    depth.set(cur + 1);
    Self {
      depth: Rc::clone(depth),
    }
  }
}

impl Drop for JsExecutionGuard {
  fn drop(&mut self) {
    let cur = self.depth.get();
    debug_assert!(cur > 0, "js execution depth underflow");
    self.depth.set(cur.saturating_sub(1));
  }
}

/// Host hook for firing DOM `load` / `error` events at `<script>` elements.
///
/// HTML queues these as *element tasks* on the DOM manipulation task source.
pub trait ScriptElementEventHost {
  fn dispatch_script_element_event(&mut self, script: NodeId, event_name: &'static str) -> Result<()>;
}

/// Options used when fetching a module script's module graph.
///
/// This is intentionally minimal today: the pipeline + scheduler are focused on deterministic
/// ordering semantics, not fetch/CORS/credentials policy. Add fields as module loading is
/// implemented in the real engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModuleGraphFetchOptions {}

/// Host interface used by [`HtmlScriptPipeline`].
pub trait HtmlScriptPipelineHost: CurrentScriptHost + DomHost + ScriptElementEventHost + Sized + 'static {
  fn start_classic_fetch(&mut self, script_id: HtmlScriptId, url: &str) -> Result<()>;

  fn start_module_graph_fetch(
    &mut self,
    script_id: HtmlScriptId,
    url: &str,
    options: ModuleGraphFetchOptions,
  ) -> Result<()>;

  fn start_inline_module_graph_fetch(
    &mut self,
    script_id: HtmlScriptId,
    source_text: &str,
    base_url: Option<&str>,
    options: ModuleGraphFetchOptions,
  ) -> Result<()>;

  fn execute_classic_script(
    &mut self,
    source_text: Option<&str>,
    script_node_id: NodeId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()>;

  fn execute_module_script(
    &mut self,
    module_handle: Option<&str>,
    script_node_id: NodeId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()>;

  fn register_import_map(
    &mut self,
    source_text: &str,
    base_url: Option<&str>,
    script_node_id: NodeId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()>;
}

struct HtmlScriptPipelineState {
  parser: StreamingHtmlParser,
  scheduler: HtmlScriptScheduler<NodeId>,
  orchestrator: Rc<RefCell<ScriptOrchestrator>>,
  document: Option<Document>,

  blocked_parser_on: Option<HtmlScriptId>,

  script_type_by_id: HashMap<HtmlScriptId, ScriptType>,
  external_file_by_id: HashMap<HtmlScriptId, bool>,

  js_execution_depth: Rc<Cell<usize>>,

  parse_budget: ParseBudget,
  parse_task_scheduled: bool,
}

impl HtmlScriptPipelineState {
  fn new(document_url: Option<&str>, parse_budget: ParseBudget) -> Self {
    Self {
      parser: StreamingHtmlParser::new(document_url),
      scheduler: HtmlScriptScheduler::new(),
      orchestrator: Rc::new(RefCell::new(ScriptOrchestrator::new())),
      document: None,
      blocked_parser_on: None,
      script_type_by_id: HashMap::new(),
      external_file_by_id: HashMap::new(),
      js_execution_depth: Rc::new(Cell::new(0)),
      parse_budget,
      parse_task_scheduled: false,
    }
  }

  fn parsing_finished(&self) -> bool {
    self.document.is_some()
  }

  fn on_classic_fetch_completed<Host: HtmlScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    script_id: HtmlScriptId,
    source_text: &str,
  ) -> Result<bool> {
    let was_blocked = self.blocked_parser_on.is_some();
    let actions = self
      .scheduler
      .classic_fetch_completed(script_id, source_text.to_string())?;
    self.apply_actions(host, event_loop, actions)?;
    Ok(was_blocked && self.blocked_parser_on.is_none())
  }

  fn on_module_graph_completed<Host: HtmlScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    script_id: HtmlScriptId,
    module_handle: String,
  ) -> Result<()> {
    let actions = self.scheduler.module_graph_completed(script_id, module_handle)?;
    self.apply_actions(host, event_loop, actions)?;
    Ok(())
  }

  fn parse_task<Host: HtmlScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<bool> {
    if self.parsing_finished() || self.blocked_parser_on.is_some() {
      return Ok(false);
    }

    let mut remaining = self.parse_budget.max_pump_iterations;
    while remaining > 0 {
      if self.blocked_parser_on.is_some() {
        return Ok(false);
      }

      match self.parser.pump()? {
        StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        } => {
          self.on_script_boundary(host, event_loop, script, base_url_at_this_point)?;
          remaining -= 1;
        }
        StreamingParserYield::NeedMoreInput => return Ok(false),
        StreamingParserYield::Finished { document } => {
          self.document = Some(document);
          let actions = self.scheduler.parsing_completed()?;
          self.apply_actions(host, event_loop, actions)?;
          return Ok(false);
        }
      }
    }

    Ok(!self.parsing_finished() && self.blocked_parser_on.is_none())
  }

  fn on_script_boundary<Host: HtmlScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    script_node_id: NodeId,
    base_url_at_discovery: Option<String>,
  ) -> Result<()> {
    // HTML: When a parser-inserted script end tag is seen, perform a microtask checkpoint *before*
    // preparing the script, but only when the JS execution context stack is empty.
    if self.js_execution_depth.get() == 0 {
      event_loop.perform_microtask_checkpoint(host)?;
    }

    let (script_type, external_file, discovered) = {
      let dom = self.parser.document().ok_or_else(|| {
        Error::Other("internal error: parser document unavailable at script boundary".to_string())
      })?;
      let base_tracker = BaseUrlTracker::new(base_url_at_discovery.as_deref());
      let spec = build_parser_inserted_script_element_spec_dom2(&dom, script_node_id, &base_tracker);
      let script_type = spec.script_type;
      let external_file = spec.src_attr_present;
      let base_url_at_discovery = spec.base_url.clone();
      let discovered = self
        .scheduler
        .discovered_parser_script(spec, script_node_id, base_url_at_discovery)?;
      (script_type, external_file, discovered)
    };

    self.script_type_by_id.insert(discovered.id, script_type);
    self.external_file_by_id
      .insert(discovered.id, external_file);

    self.apply_actions(host, event_loop, discovered.actions)?;
    Ok(())
  }

  fn queue_script_event_task<Host: HtmlScriptPipelineHost>(
    &mut self,
    event_loop: &mut EventLoop<Host>,
    node_id: NodeId,
    event: ScriptEventKind,
  ) -> Result<()> {
    let event_name: &'static str = match event {
      ScriptEventKind::Load => "load",
      ScriptEventKind::Error => "error",
    };
    event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
      with_event_loop(event_loop, || host.dispatch_script_element_event(node_id, event_name))?;
      Ok(())
    })
  }

  fn work_script_type(work: &HtmlScriptWork) -> ScriptType {
    match work {
      HtmlScriptWork::Classic { .. } => ScriptType::Classic,
      HtmlScriptWork::Module { .. } => ScriptType::Module,
      HtmlScriptWork::ImportMap { .. } => ScriptType::ImportMap,
    }
  }

  fn work_has_source(work: &HtmlScriptWork) -> Option<bool> {
    match work {
      HtmlScriptWork::Classic { source_text } => Some(source_text.is_some()),
      HtmlScriptWork::Module { source_text } => Some(source_text.is_some()),
      HtmlScriptWork::ImportMap { .. } => None,
    }
  }

  fn apply_actions<Host: HtmlScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    actions: Vec<HtmlScriptSchedulerAction<NodeId>>,
  ) -> Result<()> {
    for action in actions {
      match action {
        HtmlScriptSchedulerAction::StartClassicFetch { script_id, url, .. } => {
          host.start_classic_fetch(script_id, &url)?;
        }
        HtmlScriptSchedulerAction::StartModuleGraphFetch { script_id, url, .. } => {
          host.start_module_graph_fetch(script_id, &url, ModuleGraphFetchOptions::default())?;
        }
        HtmlScriptSchedulerAction::StartInlineModuleGraphFetch {
          script_id,
          source_text,
          base_url,
          ..
        } => {
          host.start_inline_module_graph_fetch(
            script_id,
            &source_text,
            base_url.as_deref(),
            ModuleGraphFetchOptions::default(),
          )?;
        }
        HtmlScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
          self.blocked_parser_on = Some(script_id);
        }
        HtmlScriptSchedulerAction::ExecuteNow {
          script_id,
          node_id,
          work,
        } => {
          let external_file = *self.external_file_by_id.get(&script_id).unwrap_or(&false);
          let event_kind = external_file.then(|| {
            if Self::work_has_source(&work).unwrap_or(true) {
              ScriptEventKind::Load
            } else {
              ScriptEventKind::Error
            }
          });

          {
            let _guard = JsExecutionGuard::enter(&self.js_execution_depth);
            self.execute_work_now(host, event_loop, script_id, node_id, &work)?;
          }

          if let Some(event_kind) = event_kind {
            self.queue_script_event_task(event_loop, node_id, event_kind)?;
          }

          // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
          // execution context stack is empty. Nested (re-entrant) script execution must not drain
          // microtasks until the outermost script returns.
          if self.js_execution_depth.get() == 0 {
            event_loop.perform_microtask_checkpoint(host)?;
          }
        }
        HtmlScriptSchedulerAction::QueueTask {
          script_id,
          node_id,
          work,
        } => {
          let external_file = *self.external_file_by_id.get(&script_id).unwrap_or(&false);
          let event_kind = external_file.then(|| {
            if Self::work_has_source(&work).unwrap_or(true) {
              ScriptEventKind::Load
            } else {
              ScriptEventKind::Error
            }
          });
          self.queue_work_task(event_loop, script_id, node_id, work, event_kind)?;
        }
        HtmlScriptSchedulerAction::QueueScriptEventTask { node_id, event, .. } => {
          self.queue_script_event_task(event_loop, node_id, event)?;
        }
      }
    }
    Ok(())
  }

  fn execute_work_now<Host: HtmlScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    script_id: HtmlScriptId,
    script_node_id: NodeId,
    work: &HtmlScriptWork,
  ) -> Result<()> {
    let script_type = *self
      .script_type_by_id
      .get(&script_id)
      .unwrap_or(&Self::work_script_type(work));

    let mut document = if let Some(doc) = self.document.as_ref() {
      doc.clone()
    } else {
      let dom = self.parser.document().ok_or_else(|| {
        Error::Other("internal error: parser document unavailable when executing script".to_string())
      })?;
      Document::clone(&dom)
    };
    // Ensure declarative Shadow DOM is attached before any connected script observes the tree.
    document.attach_shadow_roots();
    host.mutate_dom(|dom| {
      *dom = document;
      ((), true)
    });

    let mut orchestrator = self.orchestrator.borrow_mut();
    let mut exec = HostExecutor { work, event_loop };
    orchestrator.execute_script_element(host, script_node_id, script_type, &mut exec)?;

    if self.blocked_parser_on == Some(script_id) {
      self.blocked_parser_on = None;
    }
    Ok(())
  }

  fn queue_work_task<Host: HtmlScriptPipelineHost>(
    &mut self,
    event_loop: &mut EventLoop<Host>,
    script_id: HtmlScriptId,
    script_node_id: NodeId,
    work: HtmlScriptWork,
    event_kind: Option<ScriptEventKind>,
  ) -> Result<()> {
    let script_type = *self
      .script_type_by_id
      .get(&script_id)
      .unwrap_or(&Self::work_script_type(&work));

    let mut document = if let Some(doc) = self.document.as_ref() {
      doc.clone()
    } else {
      let dom = self.parser.document().ok_or_else(|| {
        Error::Other("internal error: parser document unavailable when queuing script task".to_string())
      })?;
      Document::clone(&dom)
    };
    document.attach_shadow_roots();

    let orchestrator = Rc::clone(&self.orchestrator);
    let js_execution_depth = Rc::clone(&self.js_execution_depth);
    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      let _guard = JsExecutionGuard::enter(&js_execution_depth);
      host.mutate_dom(|dom| {
        *dom = document.clone();
        ((), true)
      });
      {
        let mut orchestrator = orchestrator.borrow_mut();
        let mut exec = HostExecutor {
          work: &work,
          event_loop,
        };
        orchestrator.execute_script_element(host, script_node_id, script_type, &mut exec)?;
      }

      if let Some(event_kind) = event_kind {
        let event_name: &'static str = match event_kind {
          ScriptEventKind::Load => "load",
          ScriptEventKind::Error => "error",
        };
        event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
          with_event_loop(event_loop, || host.dispatch_script_element_event(script_node_id, event_name))?;
          Ok(())
        })?;
      }

      Ok(())
    })?;
    Ok(())
  }
}

/// End-to-end streaming parser + script scheduler + event loop driver.
pub struct HtmlScriptPipeline<Host: HtmlScriptPipelineHost> {
  state: Rc<RefCell<HtmlScriptPipelineState>>,
  event_loop: EventLoop<Host>,
}

impl<Host: HtmlScriptPipelineHost> HtmlScriptPipeline<Host> {
  pub fn new(document_url: Option<&str>) -> Self {
    Self::new_with_parse_budget(document_url, ParseBudget::default())
  }

  pub fn new_with_parse_budget(document_url: Option<&str>, parse_budget: ParseBudget) -> Self {
    Self {
      state: Rc::new(RefCell::new(HtmlScriptPipelineState::new(
        document_url,
        parse_budget,
      ))),
      event_loop: EventLoop::new(),
    }
  }

  pub fn event_loop(&mut self) -> &mut EventLoop<Host> {
    &mut self.event_loop
  }

  pub fn parsing_finished(&self) -> bool {
    self.state.borrow().parsing_finished()
  }

  pub fn finished_document(&self) -> Option<Document> {
    self.state.borrow().document.clone()
  }

  pub fn blocked_on_script(&self) -> Option<HtmlScriptId> {
    self.state.borrow().blocked_parser_on
  }

  pub fn feed_str(&mut self, chunk: &str) -> Result<()> {
    self.state.borrow_mut().parser.push_str(chunk);
    self.queue_parse_task()
  }

  pub fn finish_input(&mut self) -> Result<()> {
    self.state.borrow_mut().parser.set_eof();
    self.queue_parse_task()
  }

  fn queue_parse_task(&mut self) -> Result<()> {
    Self::queue_parse_task_rc(&self.state, &mut self.event_loop)
  }

  fn queue_parse_task_rc(
    state: &Rc<RefCell<HtmlScriptPipelineState>>,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    {
      let mut s = state.borrow_mut();
      if s.parsing_finished() || s.parse_task_scheduled || s.blocked_parser_on.is_some() {
        return Ok(());
      }
      s.parse_task_scheduled = true;
    }

    let state_for_task = Rc::clone(state);
    if let Err(err) = event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
      let result = {
        let mut s = state_for_task.borrow_mut();
        s.parse_task(host, event_loop)
      };
      {
        let mut s = state_for_task.borrow_mut();
        s.parse_task_scheduled = false;
      }
      let should_reschedule = result?;
      if should_reschedule {
        HtmlScriptPipeline::<Host>::queue_parse_task_rc(&state_for_task, event_loop)?;
      }
      Ok(())
    }) {
      state.borrow_mut().parse_task_scheduled = false;
      return Err(err);
    }

    Ok(())
  }

  pub fn on_classic_fetch_completed(
    &mut self,
    host: &mut Host,
    script_id: HtmlScriptId,
    source_text: &str,
  ) -> Result<()> {
    let should_resume = {
      let mut s = self.state.borrow_mut();
      s.on_classic_fetch_completed(host, &mut self.event_loop, script_id, source_text)?
    };
    if should_resume {
      self.queue_parse_task()?;
    }
    Ok(())
  }

  pub fn queue_classic_fetch_completion(
    &mut self,
    script_id: HtmlScriptId,
    source_text: String,
  ) -> Result<()> {
    let state = Rc::clone(&self.state);
    self.event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      let should_resume = {
        let mut s = state.borrow_mut();
        s.on_classic_fetch_completed(host, event_loop, script_id, &source_text)?
      };
      if should_resume {
        HtmlScriptPipeline::<Host>::queue_parse_task_rc(&state, event_loop)?;
      }
      Ok(())
    })
  }

  pub fn on_module_graph_completed(
    &mut self,
    host: &mut Host,
    script_id: HtmlScriptId,
    module_handle: String,
  ) -> Result<()> {
    let mut s = self.state.borrow_mut();
    s.on_module_graph_completed(host, &mut self.event_loop, script_id, module_handle)
  }

  pub fn queue_module_graph_completion(
    &mut self,
    script_id: HtmlScriptId,
    module_handle: String,
  ) -> Result<()> {
    let state = Rc::clone(&self.state);
    self.event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      let mut s = state.borrow_mut();
      s.on_module_graph_completed(host, event_loop, script_id, module_handle)
    })
  }
}

struct HostExecutor<'a, Host: HtmlScriptPipelineHost> {
  work: &'a HtmlScriptWork,
  event_loop: &'a mut EventLoop<Host>,
}

impl<Host: HtmlScriptPipelineHost> ScriptBlockExecutor<Host> for HostExecutor<'_, Host> {
  fn execute_script(
    &mut self,
    host: &mut Host,
    _orchestrator: &mut ScriptOrchestrator,
    script: NodeId,
    script_type: ScriptType,
  ) -> Result<()> {
    match self.work {
      HtmlScriptWork::Classic { source_text } => {
        debug_assert_eq!(script_type, ScriptType::Classic);
        host.execute_classic_script(source_text.as_deref(), script, self.event_loop)
      }
      HtmlScriptWork::Module { source_text } => {
        debug_assert_eq!(script_type, ScriptType::Module);
        host.execute_module_script(source_text.as_deref(), script, self.event_loop)
      }
      HtmlScriptWork::ImportMap { source_text, base_url } => {
        debug_assert_eq!(script_type, ScriptType::ImportMap);
        host.register_import_map(source_text, base_url.as_deref(), script, self.event_loop)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::{CurrentScriptStateHandle, RunLimits};
  use selectors::context::QuirksMode;

  struct Host {
    dom: Document,
    current_script: CurrentScriptStateHandle,
    started_classic_fetches: Vec<(HtmlScriptId, String)>,
    started_module_fetches: Vec<(HtmlScriptId, String)>,
    started_inline_module_fetches: Vec<(HtmlScriptId, String)>,
    log: Vec<String>,
  }

  impl Default for Host {
    fn default() -> Self {
      Self {
        dom: Document::new(QuirksMode::NoQuirks),
        current_script: CurrentScriptStateHandle::default(),
        started_classic_fetches: Vec::new(),
        started_module_fetches: Vec::new(),
        started_inline_module_fetches: Vec::new(),
        log: Vec::new(),
      }
    }
  }

  impl DomHost for Host {
    fn with_dom<R, F>(&self, f: F) -> R
    where
      F: FnOnce(&Document) -> R,
    {
      f(&self.dom)
    }

    fn mutate_dom<R, F>(&mut self, f: F) -> R
    where
      F: FnOnce(&mut Document) -> (R, bool),
    {
      let (result, _changed) = f(&mut self.dom);
      result
    }
  }

  impl CurrentScriptHost for Host {
    fn current_script_state(&self) -> &CurrentScriptStateHandle {
      &self.current_script
    }
  }

  impl ScriptElementEventHost for Host {
    fn dispatch_script_element_event(
      &mut self,
      _script: NodeId,
      _event_name: &'static str,
    ) -> Result<()> {
      Ok(())
    }
  }

  impl HtmlScriptPipelineHost for Host {
    fn start_classic_fetch(&mut self, script_id: HtmlScriptId, url: &str) -> Result<()> {
      self.started_classic_fetches.push((script_id, url.to_string()));
      Ok(())
    }

    fn start_module_graph_fetch(
      &mut self,
      script_id: HtmlScriptId,
      url: &str,
      _options: ModuleGraphFetchOptions,
    ) -> Result<()> {
      self.started_module_fetches.push((script_id, url.to_string()));
      Ok(())
    }

    fn start_inline_module_graph_fetch(
      &mut self,
      script_id: HtmlScriptId,
      source_text: &str,
      _base_url: Option<&str>,
      _options: ModuleGraphFetchOptions,
    ) -> Result<()> {
      self
        .started_inline_module_fetches
        .push((script_id, source_text.to_string()));
      Ok(())
    }

    fn execute_classic_script(
      &mut self,
      source_text: Option<&str>,
      script_node_id: NodeId,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      assert_eq!(
        self.current_script(),
        Some(script_node_id),
        "expected classic script to set document.currentScript"
      );
      let body = source_text.unwrap_or("<null>");
      self.log.push(format!("classic:{body}"));
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      module_handle: Option<&str>,
      _script_node_id: NodeId,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      assert_eq!(
        self.current_script(),
        None,
        "module scripts must observe document.currentScript == null"
      );
      let body = module_handle.unwrap_or("<null>");
      self.log.push(format!("module:{body}"));
      Ok(())
    }

    fn register_import_map(
      &mut self,
      source_text: &str,
      _base_url: Option<&str>,
      _script_node_id: NodeId,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      assert_eq!(
        self.current_script(),
        None,
        "import maps must not set document.currentScript"
      );
      self.log.push(format!("importmap:{source_text}"));
      Ok(())
    }
  }

  #[test]
  fn importmap_executes_synchronously_at_boundary_and_does_not_block_parsing() -> Result<()> {
    let mut host = Host::default();
    let mut p = HtmlScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str(
      r#"<!doctype html><script type=importmap>{"imports":{}}</script><script>RUN</script>"#,
    )?;
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(
      host.log,
      vec![
        r#"importmap:{"imports":{}}"#.to_string(),
        "classic:RUN".to_string(),
      ]
    );
    assert!(p.parsing_finished());
    assert!(p.blocked_on_script().is_none());
    Ok(())
  }

  #[test]
  fn deferred_module_scripts_execute_after_parsing_complete_in_document_order() -> Result<()> {
    let mut host = Host::default();
    let mut p = HtmlScriptPipeline::<Host>::new_with_parse_budget(
      Some("https://ex/doc.html"),
      ParseBudget::new(1),
    );

    p.feed_str(
      r#"<script type=module src="/m1.js"></script><script type=module src="/m2.js"></script>"#,
    )?;
    // No EOF yet, so parsing cannot finish. Discover scripts and start module fetches.
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert!(!p.parsing_finished());
    assert_eq!(host.started_module_fetches.len(), 2);

    let m1 = host.started_module_fetches[0].0;
    let m2 = host.started_module_fetches[1].0;

    // Complete out-of-order before parsing completes.
    p.on_module_graph_completed(&mut host, m2, "m2".to_string())?;
    p.on_module_graph_completed(&mut host, m1, "m1".to_string())?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert!(
      host.log.is_empty(),
      "deferred module scripts must not execute before parsing completes"
    );

    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.log,
      vec!["module:m1".to_string(), "module:m2".to_string()]
    );
    Ok(())
  }

  #[test]
  fn deferred_inline_module_scripts_execute_after_parsing_complete_in_document_order() -> Result<()> {
    let mut host = Host::default();
    let mut p = HtmlScriptPipeline::<Host>::new_with_parse_budget(
      Some("https://ex/doc.html"),
      ParseBudget::new(1),
    );
    p.feed_str(
      r#"<script type=module>/*m1*/</script><script type=module>/*m2*/</script>"#,
    )?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert!(!p.parsing_finished());
    assert_eq!(host.started_inline_module_fetches.len(), 2);

    let m1 = host.started_inline_module_fetches[0].0;
    let m2 = host.started_inline_module_fetches[1].0;

    // Complete out-of-order before parsing completes.
    p.on_module_graph_completed(&mut host, m2, "m2".to_string())?;
    p.on_module_graph_completed(&mut host, m1, "m1".to_string())?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert!(
      host.log.is_empty(),
      "deferred module scripts must not execute before parsing completes"
    );

    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(
      host.log,
      vec!["module:m1".to_string(), "module:m2".to_string()]
    );
    Ok(())
  }

  #[test]
  fn importmap_registers_before_subsequent_module_fetch_starts() -> Result<()> {
    let mut host = Host::default();
    let mut p = HtmlScriptPipeline::<Host>::new_with_parse_budget(
      Some("https://ex/doc.html"),
      ParseBudget::new(1),
    );
    p.feed_str(
      r#"<script type=importmap>{"imports":{"x":"/x.js"}}</script><script type=module src="/m.js"></script>"#,
    )?;
    p.finish_input()?;

    // 1st DOMManipulation parse task: should hit the import map boundary and register it.
    assert!(p.event_loop().run_next_task(&mut host)?);
    assert_eq!(host.log, vec![r#"importmap:{"imports":{"x":"/x.js"}}"#.to_string()]);
    assert!(
      host.started_module_fetches.is_empty(),
      "module fetch must not start until after import map is processed"
    );

    // 2nd DOMManipulation parse task: should reach the module script boundary and start its fetch.
    assert!(p.event_loop().run_next_task(&mut host)?);
    assert_eq!(host.started_module_fetches.len(), 1);

    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert!(p.parsing_finished());
    Ok(())
  }

  #[test]
  fn async_module_scripts_can_execute_before_parsing_finishes() -> Result<()> {
    let mut host = Host::default();
    let mut p = HtmlScriptPipeline::<Host>::new_with_parse_budget(
      Some("https://ex/doc.html"),
      ParseBudget::new(1),
    );

    p.feed_str(r#"<script type=module async src="/a.js"></script>"#)?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert!(!p.parsing_finished());
    assert_eq!(host.started_module_fetches.len(), 1);
    let async_id = host.started_module_fetches[0].0;

    p.queue_module_graph_completion(async_id, "A".to_string())?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["module:A".to_string()]);
    assert!(
      !p.parsing_finished(),
      "async module scripts should be able to run before EOF"
    );
    Ok(())
  }
}
