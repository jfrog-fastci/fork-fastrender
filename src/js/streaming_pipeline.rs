//! Temporary end-to-end integration harness for classic `<script>` processing.
//!
//! This module ties together the core building blocks required for spec-correct HTML classic script
//! execution:
//! - streaming HTML parsing that pauses at `</script>` (`html::streaming_parser`),
//! - parse-time [`ScriptElementSpec`] construction using parse-time base URL timing,
//! - the state-machine [`ScriptScheduler`] action stream,
//! - script execution via [`ScriptOrchestrator`] (for `Document.currentScript` bookkeeping),
//! - and microtask checkpoints via [`EventLoop`].
//!
//! It is intentionally **self-contained** and **test-driven** so we can validate the architecture
//! before wiring in the real network stack and JS engine.

use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::Result;
use crate::html::base_url_tracker::resolve_script_src_at_parse_time;
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};

use super::DomHost;
use super::orchestrator::{CurrentScriptHost, ScriptBlockExecutor, ScriptOrchestrator};
use super::script_scheduler::{ScriptId, ScriptScheduler, ScriptSchedulerAction};
use super::{determine_script_type_dom2, ScriptElementSpec, ScriptType};
use super::{EventLoop, TaskSource};

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Configures how much parsing work is performed per event-loop "parse task".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseBudget {
  /// Maximum number of [`StreamingHtmlParser::pump`] iterations performed in a single parse task.
  pub max_pump_iterations: usize,
}

impl ParseBudget {
  pub fn new(max_pump_iterations: usize) -> Self {
    Self {
      max_pump_iterations: max_pump_iterations.max(1),
    }
  }
}

impl Default for ParseBudget {
  fn default() -> Self {
    // Keep tasks small so other queued tasks (e.g. async script execution) can interleave.
    Self {
      max_pump_iterations: 64,
    }
  }
}
/// Host interface used by [`ClassicScriptPipeline`].
///
/// This is an MVP bridge between the scheduler state machine and an eventual real networking + JS
/// runtime integration.
pub trait ClassicScriptPipelineHost: CurrentScriptHost + DomHost + Sized + 'static {
  /// Begin fetching an external script resource.
  ///
  /// Tests typically record the request and call [`ClassicScriptPipeline::on_fetch_completed`]
  /// manually.
  fn start_fetch(&mut self, script_id: ScriptId, url: &str) -> Result<()>;

  /// Execute a script block.
  ///
  /// The implementation may queue microtasks via `event_loop.queue_microtask(...)`.
  fn execute_script(
    &mut self,
    source_text: &str,
    script_node_id: NodeId,
    script_type: ScriptType,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()>;
}

/// Progress result for [`ClassicScriptPipeline::pump_parser`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpOutcome {
  /// Parsing is blocked on a parser-blocking external script.
  Blocked,
  /// Parser ran out of buffered input.
  NeedMoreInput,
  /// Parsing reached EOF and `scheduler.parsing_completed()` has been processed.
  Finished,
}

struct ClassicScriptPipelineState {
  parser: StreamingHtmlParser,
  scheduler: ScriptScheduler<NodeId>,
  orchestrator: Rc<RefCell<ScriptOrchestrator>>,
  document: Option<Document>,
  blocked_parser_on: Option<ScriptId>,
  script_node_by_id: HashMap<ScriptId, NodeId>,
  script_type_by_id: HashMap<ScriptId, ScriptType>,

  parse_budget: ParseBudget,
  parse_task_scheduled: bool,
}

impl ClassicScriptPipelineState {
  fn new(document_url: Option<&str>, parse_budget: ParseBudget) -> Self {
    Self {
      parser: StreamingHtmlParser::new(document_url),
      scheduler: ScriptScheduler::new(),
      orchestrator: Rc::new(RefCell::new(ScriptOrchestrator::new())),
      document: None,
      blocked_parser_on: None,
      script_node_by_id: HashMap::new(),
      script_type_by_id: HashMap::new(),
      parse_budget,
      parse_task_scheduled: false,
    }
  }

  fn parsing_finished(&self) -> bool {
    self.document.is_some()
  }

  fn on_fetch_completed<Host: ClassicScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    script_id: ScriptId,
    source_text: &str,
  ) -> Result<bool> {
    let was_blocked = self.blocked_parser_on.is_some();
    let actions = self
      .scheduler
      .fetch_completed(script_id, source_text.to_string())?;
    self.apply_actions(host, event_loop, actions)?;
    Ok(was_blocked && self.blocked_parser_on.is_none())
  }

  fn parse_task<Host: ClassicScriptPipelineHost>(
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

      match self.parser.pump() {
        StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        } => {
          self.on_script_boundary(host, event_loop, script, base_url_at_this_point)?;
          remaining -= 1;
          continue;
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

    // Budget exhausted: yield back to the event loop and continue parsing in another task.
    Ok(!self.parsing_finished() && self.blocked_parser_on.is_none())
  }

  fn on_script_boundary<Host: ClassicScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    script_node_id: NodeId,
    base_url_at_discovery: Option<String>,
  ) -> Result<()> {
    // Scope the `document()` borrow so we can later mutably borrow `self` when applying actions.
    let (spec, discovered) = {
      let dom = self.parser.document();
      let spec = self.build_script_element_spec(&dom, script_node_id, base_url_at_discovery);
      let base_url_at_discovery = spec.base_url.clone();
      let discovered = self.scheduler.discovered_parser_script(
        spec.clone(),
        script_node_id,
        base_url_at_discovery,
      )?;
      (spec, discovered)
    };

    self
      .script_node_by_id
      .insert(discovered.id, script_node_id);
    self
      .script_type_by_id
      .insert(discovered.id, spec.script_type);

    self.apply_actions(host, event_loop, discovered.actions)?;
    Ok(())
  }

  fn build_script_element_spec(
    &self,
    dom: &Document,
    script_node_id: NodeId,
    base_url: Option<String>,
  ) -> ScriptElementSpec {
    let NodeKind::Element {
      tag_name,
      namespace,
      ..
    } = &dom.node(script_node_id).kind
    else {
      return ScriptElementSpec {
        base_url,
        src: None,
        inline_text: String::new(),
        async_attr: false,
        defer_attr: false,
        parser_inserted: true,
        script_type: ScriptType::Unknown,
      };
    };

    // Only scripts in the HTML namespace participate in the HTML script processing model.
    if !tag_name.eq_ignore_ascii_case("script")
      || !(namespace.is_empty() || namespace == HTML_NAMESPACE)
    {
      return ScriptElementSpec {
        base_url,
        src: None,
        inline_text: String::new(),
        async_attr: false,
        defer_attr: false,
        parser_inserted: true,
        script_type: ScriptType::Unknown,
      };
    }

    // HTML: "prepare a script" early-outs when the script element is not connected.
    if !dom.is_connected_for_scripting(script_node_id) {
      return ScriptElementSpec {
        base_url,
        src: None,
        inline_text: String::new(),
        async_attr: false,
        defer_attr: false,
        parser_inserted: true,
        script_type: ScriptType::Unknown,
      };
    }

    let async_attr = dom.has_attribute(script_node_id, "async").unwrap_or(false);
    let defer_attr = dom.has_attribute(script_node_id, "defer").unwrap_or(false);
    let raw_src = dom
      .get_attribute(script_node_id, "src")
      .ok()
      .flatten()
      .map(|v| v.to_string());
    let src =
      raw_src.as_deref().and_then(|raw| resolve_script_src_at_parse_time(base_url.as_deref(), raw));

    let inline_text = {
      let mut out = String::new();
      for &child in dom.node(script_node_id).children.iter() {
        if let NodeKind::Text { content } = &dom.node(child).kind {
          out.push_str(content);
        }
      }
      out
    };

    // Determine script type from the real dom2 node attributes (avoid allocating a legacy DomNode
    // wrapper).
    let script_type = determine_script_type_dom2(dom, script_node_id);

    ScriptElementSpec {
      base_url,
      src,
      inline_text,
      async_attr,
      defer_attr,
      parser_inserted: true,
      script_type,
    }
  }

  fn apply_actions<Host: ClassicScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    actions: Vec<ScriptSchedulerAction<NodeId>>,
  ) -> Result<()> {
    for action in actions {
      match action {
        ScriptSchedulerAction::StartFetch { script_id, url, .. } => {
          host.start_fetch(script_id, &url)?;
        }
        ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
          self.blocked_parser_on = Some(script_id);
        }
        ScriptSchedulerAction::ExecuteNow {
          script_id,
          source_text,
          ..
        } => {
          self.execute_script_now(host, event_loop, script_id, &source_text)?;
          // Parser-blocking scripts must run an explicit microtask checkpoint before parsing resumes.
          event_loop.perform_microtask_checkpoint(host)?;
        }
        ScriptSchedulerAction::QueueTask {
          script_id,
          source_text,
          ..
        } => {
          self.queue_script_task(event_loop, script_id, source_text)?;
        }
      }
    }
    Ok(())
  }

  fn execute_script_now<Host: ClassicScriptPipelineHost>(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    script_id: ScriptId,
    source_text: &str,
  ) -> Result<()> {
    let script_node_id = *self
      .script_node_by_id
      .get(&script_id)
      .expect("execute_script_now requires script node id");
    let script_type = *self
      .script_type_by_id
      .get(&script_id)
      .unwrap_or(&ScriptType::Classic);

    let mut orchestrator = self.orchestrator.borrow_mut();
    let mut exec = HostExecutor {
      source_text,
      event_loop,
    };
    if let Some(doc) = self.document.as_ref() {
      let document = doc.clone();
      host.mutate_dom(|dom| {
        *dom = document;
        ((), true)
      });
      orchestrator.execute_script_element(host, script_node_id, script_type, &mut exec)?;
    } else {
      let dom = self.parser.document();
      let document = Document::clone(&dom);
      host.mutate_dom(|dom| {
        *dom = document;
        ((), true)
      });
      orchestrator.execute_script_element(host, script_node_id, script_type, &mut exec)?;
    }

    if self.blocked_parser_on == Some(script_id) {
      self.blocked_parser_on = None;
    }
    Ok(())
  }

  fn queue_script_task<Host: ClassicScriptPipelineHost>(
    &mut self,
    event_loop: &mut EventLoop<Host>,
    script_id: ScriptId,
    source_text: String,
  ) -> Result<()> {
    let script_node_id = *self
      .script_node_by_id
      .get(&script_id)
      .expect("queue_script_task requires script node id");
    let script_type = *self
      .script_type_by_id
      .get(&script_id)
      .unwrap_or(&ScriptType::Classic);

    let document = if let Some(doc) = self.document.as_ref() {
      doc.clone()
    } else {
      let dom = self.parser.document();
      Document::clone(&dom)
    };
    let orchestrator = Rc::clone(&self.orchestrator);
    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      host.mutate_dom(|dom| {
        *dom = document.clone();
        ((), true)
      });
      let mut orchestrator = orchestrator.borrow_mut();
      let mut exec = HostExecutor {
        source_text: &source_text,
        event_loop,
      };
      orchestrator.execute_script_element(host, script_node_id, script_type, &mut exec)
    })?;
    Ok(())
  }
}

/// End-to-end streaming parse + scheduler + event loop driver (classic scripts only).
pub struct ClassicScriptPipeline<Host: ClassicScriptPipelineHost> {
  state: Rc<RefCell<ClassicScriptPipelineState>>,
  event_loop: EventLoop<Host>,
}

impl<Host: ClassicScriptPipelineHost> ClassicScriptPipeline<Host> {
  pub fn new(document_url: Option<&str>) -> Self {
    Self::new_with_parse_budget(document_url, ParseBudget::default())
  }

  pub fn new_with_parse_budget(document_url: Option<&str>, parse_budget: ParseBudget) -> Self {
    Self {
      state: Rc::new(RefCell::new(ClassicScriptPipelineState::new(
        document_url,
        parse_budget,
      ))),
      event_loop: EventLoop::new(),
    }
  }

  pub fn event_loop(&mut self) -> &mut EventLoop<Host> {
    &mut self.event_loop
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
    state: &Rc<RefCell<ClassicScriptPipelineState>>,
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
        ClassicScriptPipeline::<Host>::queue_parse_task_rc(&state_for_task, event_loop)?;
      }
      Ok(())
    }) {
      state.borrow_mut().parse_task_scheduled = false;
      return Err(err);
    }

    Ok(())
  }

  /// Notify the pipeline that a previously requested external script fetch completed.
  pub fn on_fetch_completed(
    &mut self,
    host: &mut Host,
    script_id: ScriptId,
    source_text: &str,
  ) -> Result<()> {
    let should_resume = {
      let mut s = self.state.borrow_mut();
      s.on_fetch_completed(host, &mut self.event_loop, script_id, source_text)?
    };
    if should_resume {
      self.queue_parse_task()?;
    }
    Ok(())
  }

  pub fn queue_fetch_completion(
    &mut self,
    script_id: ScriptId,
    source_text: String,
  ) -> Result<()> {
    let state = Rc::clone(&self.state);
    self
      .event_loop
      .queue_task(TaskSource::Networking, move |host, event_loop| {
        let should_resume = {
          let mut s = state.borrow_mut();
          s.on_fetch_completed(host, event_loop, script_id, &source_text)?
        };
        if should_resume {
          ClassicScriptPipeline::<Host>::queue_parse_task_rc(&state, event_loop)?;
        }
        Ok(())
      })
  }
}

struct HostExecutor<'a, Host: ClassicScriptPipelineHost> {
  source_text: &'a str,
  event_loop: &'a mut EventLoop<Host>,
}

impl<Host: ClassicScriptPipelineHost> ScriptBlockExecutor<Host> for HostExecutor<'_, Host> {
  fn execute_script(
    &mut self,
    host: &mut Host,
    _orchestrator: &mut ScriptOrchestrator,
    script: NodeId,
    script_type: ScriptType,
  ) -> Result<()> {
    host.execute_script(self.source_text, script, script_type, self.event_loop)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom::SVG_NAMESPACE;
  use crate::js::{CurrentScriptStateHandle, EventLoop, RunLimits};
  use selectors::context::QuirksMode;

  struct Host {
    dom: Document,
    current_script: CurrentScriptStateHandle,
    started_fetches: Vec<(ScriptId, String)>,
    log: Vec<String>,
    assert_dom_state_on_execute: Option<Box<dyn Fn(&Document)>>,
  }

  impl Default for Host {
    fn default() -> Self {
      Self {
        dom: Document::new(QuirksMode::NoQuirks),
        current_script: CurrentScriptStateHandle::default(),
        started_fetches: Vec::new(),
        log: Vec::new(),
        assert_dom_state_on_execute: None,
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

  impl ClassicScriptPipelineHost for Host {
    fn start_fetch(&mut self, script_id: ScriptId, url: &str) -> Result<()> {
      self.started_fetches.push((script_id, url.to_string()));
      Ok(())
    }

    fn execute_script(
      &mut self,
      source_text: &str,
      _script_node_id: NodeId,
      _script_type: ScriptType,
      event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      if let Some(assert) = &self.assert_dom_state_on_execute {
        assert(&self.dom);
      }
      self.log.push(source_text.to_string());
      let micro = format!("m{source_text}");
      event_loop.queue_microtask(move |host, _| {
        host.log.push(micro);
        Ok(())
      })?;
      Ok(())
    }
  }

  #[test]
  fn build_script_element_spec_ignores_inert_or_foreign_scripts() {
    let mut doc = Document::new(QuirksMode::NoQuirks);

    // `dom2::Document` enforces the DOM hierarchy rules for `Document` nodes (only one element child).
    // Create an `<html>` documentElement up front, then attach our test elements underneath it.
    let html = doc.create_element("html", HTML_NAMESPACE);
    doc.append_child(doc.root(), html).expect("append_child");

    let template = doc.create_element("template", HTML_NAMESPACE);
    doc.node_mut(template).inert_subtree = true;
    let inert_script = doc.create_element("script", HTML_NAMESPACE);
    doc
      .set_attribute(inert_script, "src", "inert.js")
      .expect("set_attribute");
    doc.append_child(template, inert_script).expect("append_child");
    doc.append_child(html, template).expect("append_child");

    let foreign_script = doc.create_element("script", SVG_NAMESPACE);
    doc
      .set_attribute(foreign_script, "src", "foreign.js")
      .expect("set_attribute");
    doc.append_child(html, foreign_script).expect("append_child");

    let pipeline = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    let base_url = Some("https://ex/doc.html".to_string());

    let inert_spec =
      pipeline
        .state
        .borrow()
        .build_script_element_spec(&doc, inert_script, base_url.clone());
    assert_eq!(inert_spec.script_type, ScriptType::Unknown);
    assert!(inert_spec.src.is_none());
    assert_eq!(inert_spec.inline_text, "");

    let foreign_spec = pipeline
      .state
      .borrow()
      .build_script_element_spec(&doc, foreign_script, base_url);
    assert_eq!(foreign_spec.script_type, ScriptType::Unknown);
    assert!(foreign_spec.src.is_none());
    assert_eq!(foreign_spec.inline_text, "");
  }

  #[test]
  fn inline_scripts_block_parsing_and_flush_microtasks_between() -> Result<()> {
    let mut host = Host::default();
    let mut p = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str("<script>A</script><script>B</script>")?;
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(
      host.log,
      vec!["A".to_string(), "mA".to_string(), "B".to_string(), "mB".to_string()]
    );
    Ok(())
  }

  #[test]
  fn blocking_external_script_delays_later_scripts_until_fetch_completes_and_executes() -> Result<()> {
    let mut host = Host::default();
    let mut p = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str(r#"<script src="/a.js"></script><script>INLINE</script>"#)?;
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.started_fetches.len(), 1);
    assert!(host.log.is_empty(), "blocking external script should not execute without fetch");

    let blocking_id = host.started_fetches[0].0;
    p.on_fetch_completed(&mut host, blocking_id, "A_JS")?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.log,
      vec![
        "A_JS".to_string(),
        "mA_JS".to_string(),
        "INLINE".to_string(),
        "mINLINE".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn async_external_scripts_do_not_block_parsing_and_execute_as_tasks() -> Result<()> {
    let mut host = Host::default();
    let mut p = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str(r#"<script async src="/a.js"></script><script>INLINE</script>"#)?;
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.started_fetches.len(), 1);
    // Inline script executed during parsing.
    assert_eq!(host.log, vec!["INLINE".to_string(), "mINLINE".to_string()]);

    let async_id = host.started_fetches[0].0;
    p.on_fetch_completed(&mut host, async_id, "A_JS")?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.log,
      vec![
        "INLINE".to_string(),
        "mINLINE".to_string(),
        "A_JS".to_string(),
        "mA_JS".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn defer_external_scripts_run_after_parsing_completes_in_document_order() -> Result<()> {
    let mut host = Host::default();
    let mut p = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str(r#"<script defer src="/d1.js"></script><script defer src="/d2.js"></script>"#)?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.started_fetches.len(), 2);

    // Complete out of order before parsing finishes.
    let d1 = host.started_fetches[0].0;
    let d2 = host.started_fetches[1].0;
    p.on_fetch_completed(&mut host, d2, "d2")?;
    p.on_fetch_completed(&mut host, d1, "d1")?;

    // Deferred scripts must not run before parsing completes.
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.log, Vec::<String>::new());

    // Now finish parsing: defer scripts should be queued in document order (d1, d2).
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.log,
      vec![
        "d1".to_string(),
        "md1".to_string(),
        "d2".to_string(),
        "md2".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn base_url_timing_end_to_end_script_before_base_href() -> Result<()> {
    let mut host = Host::default();
    let mut p = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str(r#"<head><script src="a.js"></script><base href="https://ex/base/"></head>"#)?;
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.started_fetches.len(), 1);
    let (_id, url) = &host.started_fetches[0];
    assert_eq!(url, "https://ex/a.js");
    Ok(())
  }

  #[test]
  fn relative_external_script_without_document_url_is_preserved_as_relative_url() -> Result<()> {
    let mut host = Host::default();
    // No `document_url` hint: the base URL is initially unknown, so a relative `src` must remain
    // relative (and must not be dropped / treated as an inline script).
    let mut p = ClassicScriptPipeline::<Host>::new(None);
    p.feed_str(r#"<script src="a.js"></script>"#)?;
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.started_fetches.len(), 1);
    let id = host.started_fetches[0].0;
    let url = host.started_fetches[0].1.clone();
    assert_eq!(url, "a.js");

    p.on_fetch_completed(&mut host, id, "A_JS")?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.log, vec!["A_JS".to_string(), "mA_JS".to_string()]);
    Ok(())
  }

  #[test]
  fn async_script_executes_before_parsing_finishes_and_flushes_microtasks() -> Result<()> {
    let mut host = Host::default();
    let mut p = ClassicScriptPipeline::<Host>::new_with_parse_budget(
      Some("https://ex/doc.html"),
      ParseBudget::new(1),
    );

    // First chunk: discover the async script and start fetch.
    p.feed_str(r#"<script async src="/a.js"></script>"#)?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.started_fetches.len(), 1);
    let async_id = host.started_fetches[0].0;

    // Parser must not have finished: no EOF yet.
    assert!(
      !p.state.borrow().parsing_finished(),
      "expected parser to be waiting for more input"
    );

    // Enqueue async fetch completion ahead of the next parse chunk.
    p.queue_fetch_completion(async_id, "A_JS".to_string())?;
    p.feed_str("<p>after</p>")?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["A_JS".to_string(), "mA_JS".to_string()]);
    assert!(
      !p.state.borrow().parsing_finished(),
      "expected parser not to be finished until EOF"
    );

    // Now end the input stream; parsing can finish.
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;
    assert!(p.state.borrow().parsing_finished());
    Ok(())
  }

  #[test]
  fn module_scripts_are_ignored_by_classic_pipeline() -> Result<()> {
    let mut host = Host::default();
    let mut p = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str(r#"<script type="module" src="/a.js"></script><script>INLINE</script>"#)?;
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    // The classic pipeline only handles classic scripts; module scripts must be ignored.
    assert!(host.started_fetches.is_empty());
    assert_eq!(host.log, vec!["INLINE".to_string(), "mINLINE".to_string()]);
    Ok(())
  }

  #[test]
  fn parser_pause_point_executes_script_before_parsing_following_markup() -> Result<()> {
    let mut host = Host::default();
    host.assert_dom_state_on_execute = Some(Box::new(|dom| {
      assert!(
        dom.get_element_by_id("after").is_none(),
        "markup after </script> must not be visible when the script executes"
      );
    }));
    let mut p = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str("<script>1</script><div id=after></div>")?;
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["1".to_string(), "m1".to_string()]);

    let after_present_in_final_doc = {
      let s = p.state.borrow();
      let final_doc = s
        .document
        .as_ref()
        .expect("expected parsing to finish");
      final_doc.get_element_by_id("after").is_some()
    };
    assert!(
      after_present_in_final_doc,
      "expected parser to resume after executing the script"
    );
    Ok(())
  }

  #[test]
  fn blocking_external_script_blocks_parser_before_following_markup_is_inserted() -> Result<()> {
    let mut host = Host::default();
    let mut p = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str(r#"<script src="/a.js"></script><div id=after></div>"#)?;
    p.finish_input()?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.started_fetches.len(), 1);
    let blocking_id = host.started_fetches[0].0;

    let after_present_while_blocked = {
      let s = p.state.borrow();
      let dom = s.parser.document();
      dom.get_element_by_id("after").is_some()
    };
    assert!(
      !after_present_while_blocked,
      "parser should not parse markup after a blocking external <script> before it executes"
    );
    assert!(
      p.state.borrow().blocked_parser_on.is_some(),
      "expected parser to be blocked on the external script fetch"
    );
    assert!(
      !p.state.borrow().parsing_finished(),
      "expected parsing not to complete until the blocking script executes"
    );

    p.on_fetch_completed(&mut host, blocking_id, "A_JS")?;
    p.event_loop().run_until_idle(&mut host, RunLimits::unbounded())?;

    let after_present_in_final_doc = {
      let s = p.state.borrow();
      let final_doc = s
        .document
        .as_ref()
        .expect("expected parsing to finish after fetch completion");
      final_doc.get_element_by_id("after").is_some()
    };
    assert!(after_present_in_final_doc);
    Ok(())
  }
}
