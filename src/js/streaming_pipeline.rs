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

use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::Result;
use crate::html::base_url_tracker::resolve_script_src_at_parse_time;
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};

use super::orchestrator::{CurrentScriptHost, ScriptBlockExecutor, ScriptOrchestrator};
use super::script_scheduler::{ScriptId, ScriptScheduler, ScriptSchedulerAction};
use super::{determine_script_type, ScriptElementSpec, ScriptType};
use super::{EventLoop, TaskSource};

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Host interface used by [`ClassicScriptPipeline`].
///
/// This is an MVP bridge between the scheduler state machine and an eventual real networking + JS
/// runtime integration.
pub trait ClassicScriptPipelineHost: CurrentScriptHost + Sized + 'static {
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

/// End-to-end streaming parse + scheduler + event loop driver (classic scripts only).
pub struct ClassicScriptPipeline<Host: ClassicScriptPipelineHost> {
  parser: StreamingHtmlParser,
  scheduler: ScriptScheduler<NodeId>,
  event_loop: EventLoop<Host>,
  orchestrator: Rc<RefCell<ScriptOrchestrator>>,
  document: Option<Document>,
  blocked_parser_on: Option<ScriptId>,
  script_node_by_id: HashMap<ScriptId, NodeId>,
  script_type_by_id: HashMap<ScriptId, ScriptType>,
}

impl<Host: ClassicScriptPipelineHost> ClassicScriptPipeline<Host> {
  pub fn new(document_url: Option<&str>) -> Self {
    let parser = StreamingHtmlParser::new(document_url);
    Self {
      parser,
      scheduler: ScriptScheduler::new(),
      event_loop: EventLoop::new(),
      orchestrator: Rc::new(RefCell::new(ScriptOrchestrator::new())),
      document: None,
      blocked_parser_on: None,
      script_node_by_id: HashMap::new(),
      script_type_by_id: HashMap::new(),
    }
  }

  pub fn event_loop(&mut self) -> &mut EventLoop<Host> {
    &mut self.event_loop
  }

  pub fn feed_str(&mut self, chunk: &str) {
    self.parser.push_str(chunk);
  }

  pub fn finish_input(&mut self) {
    self.parser.set_eof();
  }

  /// Drive the HTML parser until it blocks on a script, needs more input, or finishes.
  pub fn pump_parser(&mut self, host: &mut Host) -> Result<PumpOutcome> {
    loop {
      if self.blocked_parser_on.is_some() {
        return Ok(PumpOutcome::Blocked);
      }

      match self.parser.pump() {
        StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        } => {
          self.on_script_boundary(host, script, base_url_at_this_point)?;
          continue;
        }
        StreamingParserYield::NeedMoreInput => return Ok(PumpOutcome::NeedMoreInput),
        StreamingParserYield::Finished { document } => {
          self.document = Some(document);
          let actions = self.scheduler.parsing_completed()?;
          self.apply_actions(host, actions)?;
          return Ok(PumpOutcome::Finished);
        }
      }
    }
  }

  /// Notify the pipeline that a previously requested external script fetch completed.
  pub fn on_fetch_completed(
    &mut self,
    host: &mut Host,
    script_id: ScriptId,
    source_text: &str,
  ) -> Result<()> {
    let was_blocked = self.blocked_parser_on.is_some();
    let actions = self
      .scheduler
      .fetch_completed(script_id, source_text.to_string())?;
    self.apply_actions(host, actions)?;

    // If the fetch completion unblocked the parser (blocking external script executed), resume
    // parsing immediately so later scripts can be discovered.
    if was_blocked && self.blocked_parser_on.is_none() {
      let _ = self.pump_parser(host)?;
    }
    Ok(())
  }

  fn on_script_boundary(
    &mut self,
    host: &mut Host,
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

    self.apply_actions(host, discovered.actions)?;
    Ok(())
  }

  fn build_script_element_spec(
    &self,
    dom: &Document,
    script_node_id: NodeId,
    base_url: Option<String>,
  ) -> ScriptElementSpec {
    let async_attr = dom.has_attribute(script_node_id, "async");
    let defer_attr = dom.has_attribute(script_node_id, "defer");
    let raw_src = dom
      .get_attribute(script_node_id, "src")
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

    // Reuse the existing `determine_script_type` logic by building a tiny `DomNode` view.
    // (This keeps the type-string edge cases tested in `js/mod.rs` working here too.)
    let script_type = {
      let attrs: Vec<(String, String)> = match &dom.node(script_node_id).kind {
        NodeKind::Element { attributes, .. } => attributes.clone(),
        NodeKind::Slot { attributes, .. } => attributes.clone(),
        _ => Vec::new(),
      };
      let node = crate::dom::DomNode {
        node_type: crate::dom::DomNodeType::Element {
          tag_name: "script".to_string(),
          namespace: String::new(),
          attributes: attrs,
        },
        children: Vec::new(),
      };
      determine_script_type(&node)
    };

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

  fn apply_actions(
    &mut self,
    host: &mut Host,
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
          self.execute_script_now(host, script_id, &source_text)?;
          // Parser-blocking scripts must run an explicit microtask checkpoint before parsing resumes.
          self.event_loop.perform_microtask_checkpoint(host)?;
        }
        ScriptSchedulerAction::QueueTask {
          script_id,
          source_text,
          ..
        } => {
          self.queue_script_task(script_id, source_text)?;
        }
      }
    }
    Ok(())
  }

  fn execute_script_now(
    &mut self,
    host: &mut Host,
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
      event_loop: &mut self.event_loop,
    };
    if let Some(doc) = self.document.as_ref() {
      orchestrator.execute_script_element(host, doc, script_node_id, script_type, &mut exec)?;
    } else {
      let dom = self.parser.document();
      orchestrator.execute_script_element(host, &dom, script_node_id, script_type, &mut exec)?;
    }

    if self.blocked_parser_on == Some(script_id) {
      self.blocked_parser_on = None;
    }
    Ok(())
  }

  fn queue_script_task(&mut self, script_id: ScriptId, source_text: String) -> Result<()> {
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
    self.event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      let mut orchestrator = orchestrator.borrow_mut();
      let mut exec = HostExecutor {
        source_text: &source_text,
        event_loop,
      };
      orchestrator.execute_script_element(host, &document, script_node_id, script_type, &mut exec)
    })?;
    Ok(())
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
    _dom: &Document,
    script: NodeId,
    script_type: ScriptType,
  ) -> Result<()> {
    host.execute_script(self.source_text, script, script_type, self.event_loop)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::{CurrentScriptStateHandle, EventLoop, RunLimits};

  #[derive(Default)]
  struct Host {
    current_script: CurrentScriptStateHandle,
    started_fetches: Vec<(ScriptId, String)>,
    log: Vec<String>,
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
  fn inline_scripts_block_parsing_and_flush_microtasks_between() -> Result<()> {
    let mut host = Host::default();
    let mut p = ClassicScriptPipeline::<Host>::new(Some("https://ex/doc.html"));
    p.feed_str("<script>A</script><script>B</script>");
    p.finish_input();
    assert_eq!(p.pump_parser(&mut host)?, PumpOutcome::Finished);
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
    p.feed_str(r#"<script src="/a.js"></script><script>INLINE</script>"#);
    p.finish_input();

    assert_eq!(p.pump_parser(&mut host)?, PumpOutcome::Blocked);
    assert_eq!(host.started_fetches.len(), 1);
    assert!(host.log.is_empty(), "blocking external script should not execute without fetch");

    let blocking_id = host.started_fetches[0].0;
    p.on_fetch_completed(&mut host, blocking_id, "A_JS")?;

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
    p.feed_str(r#"<script async src="/a.js"></script><script>INLINE</script>"#);
    p.finish_input();

    assert_eq!(p.pump_parser(&mut host)?, PumpOutcome::Finished);
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
    p.feed_str(r#"<script defer src="/d1.js"></script><script defer src="/d2.js"></script>"#);

    // Discover scripts but don't finish parsing yet.
    assert_eq!(p.pump_parser(&mut host)?, PumpOutcome::NeedMoreInput);
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
    p.finish_input();
    assert_eq!(p.pump_parser(&mut host)?, PumpOutcome::Finished);
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
    p.feed_str(r#"<head><script src="a.js"></script><base href="https://ex/base/"></head>"#);
    p.finish_input();

    assert_eq!(p.pump_parser(&mut host)?, PumpOutcome::Blocked);
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
    p.feed_str(r#"<script src="a.js"></script>"#);
    p.finish_input();

    assert_eq!(p.pump_parser(&mut host)?, PumpOutcome::Blocked);
    assert_eq!(host.started_fetches.len(), 1);
    let id = host.started_fetches[0].0;
    let url = host.started_fetches[0].1.clone();
    assert_eq!(url, "a.js");

    p.on_fetch_completed(&mut host, id, "A_JS")?;
    assert_eq!(host.log, vec!["A_JS".to_string(), "mA_JS".to_string()]);
    Ok(())
  }
} 
