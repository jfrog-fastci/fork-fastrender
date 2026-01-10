use crate::dom2::NodeId;
use crate::error::{Error, Result};
use crate::js::event_loop::{EventLoop, TaskSource};
use crate::js::{ScriptElementSpec, ScriptType};
use crate::resource::FetchDestination;

use super::html_script_scheduler::{
  HtmlScriptId, HtmlScriptScheduler, HtmlScriptSchedulerAction, HtmlScriptWork, ScriptEventKind,
};

use std::collections::HashMap;

/// Host hook for firing DOM `load` / `error` events at `<script>` elements.
///
/// This is intentionally independent of JS bindings: the host-side HTML script pipeline must be
/// able to *schedule* these events in the correct task source so that future JS event listeners can
/// observe them.
pub trait ScriptElementEventHost {
  fn dispatch_script_element_event(&mut self, script: NodeId, event_name: &'static str) -> Result<()>;
}

/// Host interface used by [`HtmlScriptPipeline`].
pub trait HtmlScriptPipelineHost: ScriptElementEventHost + Sized + 'static {
  /// Begin fetching an external script resource.
  fn start_fetch(&mut self, script_id: HtmlScriptId, url: &str, destination: FetchDestination) -> Result<()>;

  /// Begin building/fetching the module graph for an inline module script.
  ///
  /// Most hosts can ignore this hook if they don't support module scripts yet; unit tests can use it
  /// to observe the scheduler's `StartInlineModuleGraphFetch` action without immediately completing
  /// the module graph.
  fn start_inline_module_graph_fetch(
    &mut self,
    _script_id: HtmlScriptId,
    _source_text: &str,
    _base_url: Option<&str>,
    _element: &ScriptElementSpec,
  ) -> Result<()> {
    Ok(())
  }

  /// Execute a classic/module script block.
  ///
  /// `source_text=None` represents "result is null" in the HTML script processing model (for
  /// example, a network error or a module graph construction failure).
  fn execute_script(
    &mut self,
    script: NodeId,
    script_type: ScriptType,
    source_text: Option<&str>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()>;
}

/// Module-capable HTML `<script>` action interpreter.
///
/// This is an early, deterministic harness intended for unit tests and for plumbing script element
/// event dispatch through the HTML task queue model.
pub struct HtmlScriptPipeline<Host: HtmlScriptPipelineHost> {
  scheduler: HtmlScriptScheduler<NodeId>,
  event_loop: EventLoop<Host>,
  registered_import_map_count: usize,
  script_type_by_id: HashMap<HtmlScriptId, ScriptType>,
  external_file_by_id: HashMap<HtmlScriptId, bool>,
}

impl<Host: HtmlScriptPipelineHost> Default for HtmlScriptPipeline<Host> {
  fn default() -> Self {
    Self {
      scheduler: HtmlScriptScheduler::new(),
      event_loop: EventLoop::new(),
      registered_import_map_count: 0,
      script_type_by_id: HashMap::new(),
      external_file_by_id: HashMap::new(),
    }
  }
}

impl<Host: HtmlScriptPipelineHost> HtmlScriptPipeline<Host> {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn event_loop(&mut self) -> &mut EventLoop<Host> {
    &mut self.event_loop
  }

  pub fn registered_import_map_count(&self) -> usize {
    self.registered_import_map_count
  }

  pub fn discovered_parser_script(
    &mut self,
    host: &mut Host,
    node_id: NodeId,
    spec: ScriptElementSpec,
  ) -> Result<HtmlScriptId> {
    let script_type = spec.script_type;
    let external_file = spec.src_attr_present;
    let base_url_at_discovery = spec.base_url.clone();
    let discovered = self
      .scheduler
      .discovered_parser_script(spec, node_id, base_url_at_discovery)?;
    let script_id = discovered.id;
    self.script_type_by_id.insert(script_id, script_type);
    self.external_file_by_id.insert(script_id, external_file);
    self.apply_actions(host, discovered.actions)?;
    Ok(script_id)
  }

  pub fn fetch_completed(&mut self, host: &mut Host, script_id: HtmlScriptId, source_text: String) -> Result<()> {
    let Some(script_type) = self.script_type_by_id.get(&script_id).copied() else {
      return Err(Error::Other(format!(
        "fetch_completed called for unknown script_id={}",
        script_id.as_u64()
      )));
    };

    let actions = match script_type {
      ScriptType::Classic => self.scheduler.classic_fetch_completed(script_id, source_text)?,
      ScriptType::Module => self.scheduler.module_graph_completed(script_id, source_text)?,
      other => {
        return Err(Error::Other(format!(
          "fetch_completed is not supported for script type {other:?} (script_id={})",
          script_id.as_u64()
        )));
      }
    };
    self.apply_actions(host, actions)
  }

  pub fn fetch_failed(&mut self, host: &mut Host, script_id: HtmlScriptId) -> Result<()> {
    let Some(script_type) = self.script_type_by_id.get(&script_id).copied() else {
      return Err(Error::Other(format!(
        "fetch_failed called for unknown script_id={}",
        script_id.as_u64()
      )));
    };

    let actions = match script_type {
      ScriptType::Classic => self.scheduler.classic_fetch_failed(script_id)?,
      ScriptType::Module => self.scheduler.module_graph_failed(script_id)?,
      other => {
        return Err(Error::Other(format!(
          "fetch_failed is not supported for script type {other:?} (script_id={})",
          script_id.as_u64()
        )));
      }
    };
    self.apply_actions(host, actions)
  }

  pub fn parsing_completed(&mut self, host: &mut Host) -> Result<()> {
    let actions = self.scheduler.parsing_completed()?;
    self.apply_actions(host, actions)
  }

  fn queue_script_event_task(
    &mut self,
    node_id: NodeId,
    event: ScriptEventKind,
  ) -> Result<()> {
    let event_name: &'static str = match event {
      ScriptEventKind::Load => "load",
      ScriptEventKind::Error => "error",
    };
    self.event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
      debug_assert_eq!(
        event_loop.currently_running_task().map(|t| t.source),
        Some(TaskSource::DOMManipulation),
        "script element event tasks must run on the DOM manipulation task source"
      );
      host.dispatch_script_element_event(node_id, event_name)?;
      Ok(())
    })
  }

  fn apply_actions(
    &mut self,
    host: &mut Host,
    actions: Vec<HtmlScriptSchedulerAction<NodeId>>,
  ) -> Result<()> {
    for action in actions {
      match action {
        HtmlScriptSchedulerAction::StartClassicFetch {
          script_id,
          url,
          destination,
          ..
        } => {
          host.start_fetch(script_id, &url, destination)?;
        }
        HtmlScriptSchedulerAction::StartModuleGraphFetch {
          script_id,
          url,
          destination,
          element: _,
          ..
        } => {
          host.start_fetch(script_id, &url, destination)?;
        }
        HtmlScriptSchedulerAction::StartInlineModuleGraphFetch {
          script_id,
          source_text,
          base_url,
          element,
          ..
        } => {
          host.start_inline_module_graph_fetch(
            script_id,
            &source_text,
            base_url.as_deref(),
            &element,
          )?;
        }
        HtmlScriptSchedulerAction::BlockParserUntilExecuted { .. } => {
          // Parser integration is out of scope for this harness.
        }
        HtmlScriptSchedulerAction::ExecuteNow {
          script_id,
          node_id,
          work,
        } => {
          let external_file = *self.external_file_by_id.get(&script_id).unwrap_or(&false);

          let (script_type, source_text) = match &work {
            HtmlScriptWork::Classic { source_text } => (ScriptType::Classic, source_text.as_deref()),
            HtmlScriptWork::Module { source_text } => (ScriptType::Module, source_text.as_deref()),
            HtmlScriptWork::ImportMap { source_text, .. } => {
              (ScriptType::ImportMap, Some(source_text.as_str()))
            }
          };

          if script_type == ScriptType::ImportMap {
            self.registered_import_map_count += 1;
          }

          let event_kind = external_file.then(|| {
            if source_text.is_some() {
              ScriptEventKind::Load
            } else {
              ScriptEventKind::Error
            }
          });

          host.execute_script(node_id, script_type, source_text, &mut self.event_loop)?;

          if let Some(event_kind) = event_kind {
            self.queue_script_event_task(node_id, event_kind)?;
          }
        }
        HtmlScriptSchedulerAction::QueueTask {
          script_id,
          node_id,
          work,
        } => {
          let external_file = *self.external_file_by_id.get(&script_id).unwrap_or(&false);

          let (script_type, source_text) = match work {
            HtmlScriptWork::Classic { source_text } => (ScriptType::Classic, source_text),
            HtmlScriptWork::Module { source_text } => (ScriptType::Module, source_text),
            HtmlScriptWork::ImportMap { source_text, .. } => {
              self.registered_import_map_count += 1;
              (ScriptType::ImportMap, Some(source_text))
            }
          };

          let event_kind = external_file.then(|| {
            if source_text.is_some() {
              ScriptEventKind::Load
            } else {
              ScriptEventKind::Error
            }
          });

          self.event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            host.execute_script(node_id, script_type, source_text.as_deref(), event_loop)?;
            if let Some(event_kind) = event_kind {
              let event_name: &'static str = match event_kind {
                ScriptEventKind::Load => "load",
                ScriptEventKind::Error => "error",
              };
              event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
                debug_assert_eq!(
                  event_loop.currently_running_task().map(|t| t.source),
                  Some(TaskSource::DOMManipulation),
                  "script element event tasks must run on the DOM manipulation task source"
                );
                host.dispatch_script_element_event(node_id, event_name)?;
                Ok(())
              })?;
            }
            Ok(())
          })?;
        }
        HtmlScriptSchedulerAction::QueueScriptEventTask { node_id, event, .. } => {
          self.queue_script_event_task(node_id, event)?;
        }
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2::Document;
  use crate::js::event_loop::RunLimits;
  use selectors::context::QuirksMode;

  #[derive(Default)]
  struct Host {
    started_fetches: Vec<(HtmlScriptId, String, FetchDestination)>,
    log: Vec<String>,
  }

  impl ScriptElementEventHost for Host {
    fn dispatch_script_element_event(&mut self, _script: NodeId, event_name: &'static str) -> Result<()> {
      self.log.push(format!("event:{event_name}"));
      Ok(())
    }
  }

  impl HtmlScriptPipelineHost for Host {
    fn start_fetch(&mut self, script_id: HtmlScriptId, url: &str, destination: FetchDestination) -> Result<()> {
      self
        .started_fetches
        .push((script_id, url.to_string(), destination));
      Ok(())
    }

    fn execute_script(
      &mut self,
      _script: NodeId,
      script_type: ScriptType,
      source_text: Option<&str>,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      let prefix = match script_type {
        ScriptType::Classic => "classic",
        ScriptType::Module => "module",
        ScriptType::ImportMap => "importmap",
        ScriptType::Unknown => "unknown",
      };
      let body = source_text.unwrap_or("<null>");
      self.log.push(format!("exec:{prefix}:{body}"));
      Ok(())
    }
  }

  fn script_node() -> NodeId {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let script = doc.create_element("script", "");
    doc.append_child(doc.root(), script).expect("append_child");
    script
  }

  #[test]
  fn importmap_with_src_queues_error_event_task_and_does_not_register_import_map() -> Result<()> {
    let mut host = Host::default();
    let mut pipeline = HtmlScriptPipeline::<Host>::new();
    let script = script_node();

    let _id = pipeline.discovered_parser_script(
      &mut host,
      script,
      ScriptElementSpec {
        base_url: None,
        // Any `src` value (including empty) must be rejected for import maps.
        src: Some("https://example.com/im.json".to_string()),
        src_attr_present: true,
        inline_text: "{}".to_string(),
        async_attr: false,
        force_async: false,
        defer_attr: false,
        nomodule_attr: false,
        crossorigin: None,
        integrity: None,
        referrer_policy: None,
        parser_inserted: true,
        node_id: Some(script),
        script_type: ScriptType::ImportMap,
      },
    )?;

    assert!(
      host.log.is_empty(),
      "event must be queued as a task, not dispatched synchronously"
    );
    assert!(host.started_fetches.is_empty(), "external import maps must not be fetched");
    assert_eq!(
      pipeline.registered_import_map_count(),
      0,
      "external import maps must not be registered"
    );

    pipeline
      .event_loop()
      .run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["event:error".to_string()]);
    Ok(())
  }

  #[test]
  fn external_module_script_success_fires_load_after_execution() -> Result<()> {
    let mut host = Host::default();
    let mut pipeline = HtmlScriptPipeline::<Host>::new();
    let script = script_node();

    let id = pipeline.discovered_parser_script(
      &mut host,
      script,
      ScriptElementSpec {
        base_url: None,
        src: Some("https://example.com/mod.js".to_string()),
        src_attr_present: true,
        inline_text: String::new(),
        async_attr: false,
        force_async: false,
        defer_attr: false,
        nomodule_attr: false,
        crossorigin: None,
        integrity: None,
        referrer_policy: None,
        parser_inserted: true,
        node_id: Some(script),
        script_type: ScriptType::Module,
      },
    )?;

    assert_eq!(host.started_fetches.len(), 1);
    assert_eq!(host.started_fetches[0].2, FetchDestination::ScriptCors);
    assert!(host.log.is_empty());

    pipeline.fetch_completed(&mut host, id, "export default 1;".to_string())?;
    // Parser-inserted module scripts are deferred by default; they should not execute until parsing completes.
    pipeline.parsing_completed(&mut host)?;

    pipeline
      .event_loop()
      .run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.log,
      vec![
        "exec:module:export default 1;".to_string(),
        "event:load".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn external_module_script_failure_fires_error_and_not_load() -> Result<()> {
    let mut host = Host::default();
    let mut pipeline = HtmlScriptPipeline::<Host>::new();
    let script = script_node();

    let id = pipeline.discovered_parser_script(
      &mut host,
      script,
      ScriptElementSpec {
        base_url: None,
        src: Some("https://example.com/mod.js".to_string()),
        src_attr_present: true,
        inline_text: String::new(),
        async_attr: false,
        force_async: false,
        defer_attr: false,
        nomodule_attr: false,
        crossorigin: None,
        integrity: None,
        referrer_policy: None,
        parser_inserted: true,
        node_id: Some(script),
        script_type: ScriptType::Module,
      },
    )?;

    assert_eq!(host.started_fetches.len(), 1);
    assert_eq!(host.started_fetches[0].2, FetchDestination::ScriptCors);

    pipeline.fetch_failed(&mut host, id)?;
    pipeline.parsing_completed(&mut host)?;

    pipeline
      .event_loop()
      .run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.log,
      vec!["exec:module:<null>".to_string(), "event:error".to_string()]
    );
    Ok(())
  }
}
