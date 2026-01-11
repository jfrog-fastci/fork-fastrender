use crate::dom2::{Document, Dom2TreeSink, NodeId};
use crate::error::{Error, Result};
use crate::html::pausable_html5ever::{Html5everPump, PausableHtml5everParser};
use crate::js::{
  DocumentLifecycle, DocumentLifecycleHost, EventLoop, LoadBlockerKind, ScriptElementSpec,
  ScriptScheduler, ScriptSchedulerAction, ScriptType, TaskSource,
};
use crate::resource::{FetchCredentialsMode, FetchDestination};

use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::ParseOpts;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

struct JsExecutionGuard {
  depth: Rc<Cell<usize>>,
}

impl Drop for JsExecutionGuard {
  fn drop(&mut self) {
    let cur = self.depth.get();
    debug_assert!(cur > 0, "js execution depth underflow");
    self.depth.set(cur.saturating_sub(1));
  }
}

/// Script fetch adapter used by [`HtmlLoadOrchestrator`].
///
/// For now this is an extremely small surface: the orchestrator issues start-fetch requests for
/// external scripts and unit tests drive completion via
/// [`HtmlLoadOrchestrator::queue_fetch_completed`].
pub trait ScriptFetcher {
  fn start_fetch(
    &mut self,
    script_id: crate::js::ScriptId,
    url: &str,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
  ) -> Result<()>;
}

/// Script execution adapter used by [`HtmlLoadOrchestrator`].
///
/// The executor runs classic scripts and may enqueue microtasks via the [`EventLoop`]. For
/// synchronous (`ExecuteNow`) execution, the orchestrator performs an explicit microtask checkpoint
/// immediately after this method returns.
pub trait ScriptExecutor<Host> {
  fn execute(&mut self, source_text: &str, event_loop: &mut EventLoop<Host>) -> Result<()>;
}

/// Single-threaded, spec-shaped HTML page-load driver:
/// streaming parse → script discovery → scheduler actions → event loop tasks.
///
/// This is intentionally minimal: it models only classic scripts and the subset of the HTML script
/// processing model implemented by [`ScriptScheduler`].
pub struct HtmlLoadOrchestrator<F, E>
where
  F: ScriptFetcher,
  E: ScriptExecutor<HtmlLoadOrchestrator<F, E>>,
{
  html: String,
  cursor: usize,
  chunk_size: usize,
  parser_needs_more_input: bool,
  eof_sent: bool,
  parser: PausableHtml5everParser<Dom2TreeSink>,
  finished_document: Option<Document>,
  scheduler: ScriptScheduler<NodeId>,
  blocked_on: Option<crate::js::ScriptId>,
  parse_task_scheduled: bool,
  fetcher: F,
  executor: E,
  script_nodes: HashMap<crate::js::ScriptId, NodeId>,
  deferred_scripts: HashSet<crate::js::ScriptId>,
  pending_script_load_blockers: HashSet<crate::js::ScriptId>,
  js_execution_depth: Rc<Cell<usize>>,
  lifecycle: DocumentLifecycle,
  lifecycle_events: Vec<String>,
  script_events: Vec<String>,
}

impl<F, E> HtmlLoadOrchestrator<F, E>
where
  F: ScriptFetcher,
  E: ScriptExecutor<HtmlLoadOrchestrator<F, E>>,
{
  pub fn new(
    html: String,
    document_url: Option<&str>,
    chunk_size: usize,
    fetcher: F,
    executor: E,
  ) -> Self {
    let opts = ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled: true,
        ..Default::default()
      },
      ..Default::default()
    };
    let sink = Dom2TreeSink::new(document_url);
    Self {
      html,
      cursor: 0,
      chunk_size: chunk_size.max(1),
      parser_needs_more_input: true,
      eof_sent: false,
      parser: PausableHtml5everParser::new_document(sink, opts),
      finished_document: None,
      scheduler: ScriptScheduler::new(),
      blocked_on: None,
      parse_task_scheduled: false,
      fetcher,
      executor,
      script_nodes: HashMap::new(),
      deferred_scripts: HashSet::new(),
      pending_script_load_blockers: HashSet::new(),
      js_execution_depth: Rc::new(Cell::new(0)),
      lifecycle: DocumentLifecycle::new(),
      lifecycle_events: Vec::new(),
      script_events: Vec::new(),
    }
  }

  pub fn finished_document(&self) -> Option<&Document> {
    self.finished_document.as_ref()
  }

  pub fn executor(&self) -> &E {
    &self.executor
  }

  pub fn executor_mut(&mut self) -> &mut E {
    &mut self.executor
  }

  pub fn fetcher(&self) -> &F {
    &self.fetcher
  }

  pub fn fetcher_mut(&mut self) -> &mut F {
    &mut self.fetcher
  }

  pub fn start(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    self.queue_parse_task(event_loop)
  }

  fn enter_js_execution(&mut self) -> JsExecutionGuard {
    let cur = self.js_execution_depth.get();
    self.js_execution_depth.set(cur + 1);
    JsExecutionGuard {
      depth: Rc::clone(&self.js_execution_depth),
    }
  }

  fn queue_parse_task(&mut self, event_loop: &mut EventLoop<Self>) -> Result<()> {
    if self.parse_task_scheduled || self.finished_document.is_some() || self.blocked_on.is_some() {
      return Ok(());
    }
    self.parse_task_scheduled = true;
    if let Err(err) = event_loop.queue_task(TaskSource::DOMManipulation, |host, event_loop| {
      let result = host.parse_one_step(event_loop);
      host.parse_task_scheduled = false;
      match result {
        Ok(should_continue) => {
          if should_continue {
            host.queue_parse_task(event_loop)?;
          }
          Ok(())
        }
        Err(err) => Err(err),
      }
    }) {
      self.parse_task_scheduled = false;
      return Err(err);
    }
    Ok(())
  }

  fn parse_one_step(&mut self, event_loop: &mut EventLoop<Self>) -> Result<bool> {
    if self.finished_document.is_some() || self.blocked_on.is_some() {
      return Ok(false);
    }

    self.maybe_feed_chunk();

    match self.parser.pump()? {
      Html5everPump::NeedMoreInput => {
        self.parser_needs_more_input = true;
        Ok(self.cursor < self.html.len() || !self.eof_sent)
      }
      Html5everPump::Script(script_node) => {
        self.handle_script_boundary(script_node, event_loop)?;
        Ok(self.blocked_on.is_none() && self.finished_document.is_none())
      }
      Html5everPump::Finished(doc) => {
        self.finished_document = Some(doc);
        let actions = self.scheduler.parsing_completed()?;
        self.apply_actions(actions, event_loop)?;
        self.notify_parsing_completed(event_loop)?;
        Ok(false)
      }
    }
  }

  fn maybe_feed_chunk(&mut self) {
    if !self.parser_needs_more_input {
      return;
    }

    if self.cursor < self.html.len() {
      let mut end = (self.cursor + self.chunk_size).min(self.html.len());
      while end > self.cursor && !self.html.is_char_boundary(end) {
        end -= 1;
      }
      if end == self.cursor {
        // Ensure forward progress even when `chunk_size` splits a multi-byte character.
        if let Some(ch) = self.html[self.cursor..].chars().next() {
          end = (self.cursor + ch.len_utf8()).min(self.html.len());
        }
      }

      let chunk = &self.html[self.cursor..end];
      self.cursor = end;
      self.parser.push_str(chunk);
      self.parser_needs_more_input = false;
    }

    if self.cursor >= self.html.len() && !self.eof_sent {
      self.parser.set_eof();
      self.eof_sent = true;
      self.parser_needs_more_input = false;
    }
  }

  fn handle_script_boundary(
    &mut self,
    script_node: NodeId,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    // HTML: When a parser-inserted script end tag is seen, perform a microtask checkpoint *before*
    // preparing the script, but only when the JS execution context stack is empty.
    //
    // Parsing can itself run inside an event-loop task (`TaskSource::DOMManipulation`), so
    // `EventLoop::currently_running_task()` is not equivalent to the JS execution context stack.
    // Track JS execution depth explicitly and gate the checkpoint on that.
    if self.js_execution_depth.get() == 0 {
      event_loop.perform_microtask_checkpoint(self)?;
    }

    let spec = self.build_script_spec(script_node)?;
    let should_run = {
      let Some(sink) = self.parser.sink() else {
        return Err(Error::Other("page_load: parser sink unavailable".to_string()));
      };
      let mut doc = sink.document_mut();
      crate::js::prepare_script_element_dom2(&mut doc, script_node, &spec)
    };
    // `prepare_script_element_dom2` returns whether the script should actually execute. However,
    // HTML still requires that certain "non-executing" cases (notably: `src` present but empty or
    // invalid) queue an `error` event task. Let the scheduler handle those cases by continuing when
    // `src` is present even if the script will not execute.
    if !should_run && !spec.src_attr_present {
      return Ok(());
    }

    // This orchestrator models only classic script execution. Import maps are not JavaScript and
    // require a dedicated host hook (they register module specifier mappings for later module
    // resolution).
    //
    // Still allow `type="importmap" src=...` to flow through the scheduler so it can queue the
    // required `error` event task for invalid `src` usage, but ignore inline import maps so we don't
    // execute their JSON source as a classic script.
    if spec.script_type == ScriptType::ImportMap && !spec.src_attr_present {
      return Ok(());
    }
    let base_url_at_discovery = self.parser.sink().and_then(|sink| sink.current_base_url());
    let is_deferred = spec.script_type == ScriptType::Classic
      && spec.src.is_some()
      && spec.defer_attr
      && !spec.is_effectively_async();
    let discovered =
      self
        .scheduler
        .discovered_parser_script(spec, script_node, base_url_at_discovery)?;
    if discovered.actions.is_empty() {
      return Ok(());
    }
    self.script_nodes.insert(discovered.id, script_node);
    if is_deferred {
      self.lifecycle.register_deferred_script();
      self.deferred_scripts.insert(discovered.id);
    }
    self.apply_actions(discovered.actions, event_loop)?;
    Ok(())
  }

  fn build_script_spec(&self, script_node: NodeId) -> Result<ScriptElementSpec> {
    let Some(sink) = self.parser.sink() else {
      return Err(Error::Other(
        "page_load: parser sink unavailable".to_string(),
      ));
    };
    let doc = sink.document();
    let base = sink.base_url_tracker();
    Ok(
      crate::js::streaming::build_parser_inserted_script_element_spec_dom2(
        &doc,
        script_node,
        &base,
      ),
    )
  }
  fn apply_actions(
    &mut self,
    actions: Vec<ScriptSchedulerAction<NodeId>>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    for action in actions {
      match action {
        ScriptSchedulerAction::StartFetch {
          script_id,
          url,
          destination,
          credentials_mode,
          ..
        } => {
          if !self.pending_script_load_blockers.insert(script_id) {
            return Err(Error::Other(format!(
              "page_load: ScriptScheduler requested StartFetch more than once for script_id={}",
              script_id.as_u64()
            )));
          }
          self
            .lifecycle
            .register_pending_load_blocker(LoadBlockerKind::Script);
          self
            .fetcher
            .start_fetch(script_id, &url, destination, credentials_mode)?;
        }
        ScriptSchedulerAction::StartModuleGraphFetch { .. } => {
          // This orchestrator does not currently support module scripts.
        }
        ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
          self.blocked_on = Some(script_id);
        }
        ScriptSchedulerAction::ExecuteNow {
          script_id,
          source_text,
          ..
        } => {
          let exec_result = {
            let _guard = self.enter_js_execution();
            let executor = &mut self.executor;
            executor.execute(&source_text, event_loop)
          };
          // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
          // execution context stack is empty. Nested (re-entrant) script execution must not drain
          // microtasks until the outermost script returns.
          if self.js_execution_depth.get() == 0 {
            event_loop.perform_microtask_checkpoint(self)?;
          }
          if self.blocked_on == Some(script_id) {
            self.blocked_on = None;
            self.queue_parse_task(event_loop)?;
          }
          if self.pending_script_load_blockers.remove(&script_id) {
            self
              .lifecycle
              .load_blocker_completed(LoadBlockerKind::Script, event_loop)?;
          }
          exec_result?;
        }
        ScriptSchedulerAction::QueueTask {
          script_id,
          source_text,
          ..
        } => {
          let is_deferred = self.deferred_scripts.contains(&script_id);
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            let _guard = host.enter_js_execution();
            let exec_result = host.executor.execute(&source_text, event_loop);
            if is_deferred {
              host.lifecycle.deferred_script_executed(event_loop)?;
            }
            if host.pending_script_load_blockers.remove(&script_id) {
              host
                .lifecycle
                .load_blocker_completed(LoadBlockerKind::Script, event_loop)?;
            }
            exec_result?;
            Ok(())
          })?;
        }
        ScriptSchedulerAction::QueueScriptEventTask { node_id, event, .. } => {
          let type_str = event.as_type_str();
          let node_idx = node_id.index();
          event_loop.queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
            host.script_events.push(format!("{type_str}@{node_idx}"));
            Ok(())
          })?;
        }
      }
    }
    Ok(())
  }

  /// Queue a networking task that delivers an external script source to the scheduler.
  ///
  /// In real integrations this is called by the fetch implementation when a response completes.
  pub fn queue_fetch_completed(
    &mut self,
    script_id: crate::js::ScriptId,
    source_text: String,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
      let actions = host.scheduler.fetch_completed(script_id, source_text)?;
      host.apply_actions(actions, event_loop)?;
      Ok(())
    })?;
    Ok(())
  }
}

impl<F, E> DocumentLifecycleHost for HtmlLoadOrchestrator<F, E>
where
  F: ScriptFetcher,
  E: ScriptExecutor<HtmlLoadOrchestrator<F, E>>,
{
  fn with_dom_mut<R>(&mut self, f: impl FnOnce(&mut Document) -> R) -> Result<R> {
    let dom = self.finished_document.as_mut().ok_or_else(|| {
      Error::Other("cannot update document.readyState before parsing completes".to_string())
    })?;
    Ok(f(dom))
  }

  fn dispatch_lifecycle_event(
    &mut self,
    target: crate::web::events::EventTargetId,
    mut event: crate::web::events::Event,
  ) -> Result<()> {
    use crate::web::events::{dispatch_event, DomError, EventListenerInvoker, ListenerId};

    struct NoopInvoker;

    impl EventListenerInvoker for NoopInvoker {
      fn invoke(
        &mut self,
        _listener_id: ListenerId,
        _event: &mut crate::web::events::Event,
      ) -> std::result::Result<(), DomError> {
        Ok(())
      }
    }

    let dom = self.finished_document.as_ref().ok_or_else(|| {
      Error::Other("cannot dispatch lifecycle event before parsing completes".to_string())
    })?;
    self.lifecycle_events.push(event.type_.clone());
    let mut invoker = NoopInvoker;
    dispatch_event(target, &mut event, dom, dom.events(), &mut invoker)
      .map(|_default_not_prevented| ())
      .map_err(|err| Error::Other(err.to_string()))
  }

  fn document_lifecycle_mut(&mut self) -> &mut DocumentLifecycle {
    &mut self.lifecycle
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::{RunLimits, SpinOutcome};

  type TestHost = HtmlLoadOrchestrator<ManualFetcher, LoggingExecutor>;

  fn spin_until_started_fetches(
    host: &mut TestHost,
    event_loop: &mut EventLoop<TestHost>,
    expected: usize,
  ) -> Result<()> {
    let outcome = event_loop.spin_until(
      host,
      RunLimits {
        max_tasks: 10_000,
        max_microtasks: 10_000,
        max_wall_time: None,
      },
      |host| host.fetcher.started.len() < expected,
    )?;
    if !matches!(outcome, SpinOutcome::ConditionMet) {
      return Err(crate::error::Error::Other(format!(
        "event loop became idle before discovering {expected} fetches (started={})",
        host.fetcher.started.len()
      )));
    }
    Ok(())
  }

  #[derive(Default)]
  struct ManualFetcher {
    started: Vec<(crate::js::ScriptId, String, FetchDestination, FetchCredentialsMode)>,
  }

  impl ScriptFetcher for ManualFetcher {
    fn start_fetch(
      &mut self,
      script_id: crate::js::ScriptId,
      url: &str,
      destination: FetchDestination,
      credentials_mode: FetchCredentialsMode,
    ) -> Result<()> {
      self
        .started
        .push((script_id, url.to_string(), destination, credentials_mode));
      Ok(())
    }
  }

  #[derive(Default)]
  struct LoggingExecutor {
    log: Vec<String>,
  }

  impl ScriptExecutor<HtmlLoadOrchestrator<ManualFetcher, LoggingExecutor>> for LoggingExecutor {
    fn execute(
      &mut self,
      source_text: &str,
      event_loop: &mut EventLoop<HtmlLoadOrchestrator<ManualFetcher, LoggingExecutor>>,
    ) -> Result<()> {
      self.log.push(format!("script:{source_text}"));
      let name = source_text.to_string();
      event_loop.queue_microtask(move |host, _event_loop| {
        host.executor.log.push(format!("microtask:{name}"));
        Ok(())
      })?;
      Ok(())
    }
  }

  #[test]
  fn inline_scripts_execute_in_order_and_flush_microtasks_between() -> Result<()> {
    let html = "<!doctype html><script>a</script><script>b</script>".to_string();
    let mut host = TestHost::new(
      html,
      None,
      8,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.executor.log,
      vec![
        "script:a".to_string(),
        "microtask:a".to_string(),
        "script:b".to_string(),
        "microtask:b".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn inline_importmap_is_ignored_and_does_not_execute_as_classic_script() -> Result<()> {
    let html =
      "<!doctype html><script type=\"importmap\">{\"imports\":{}}</script><script>a</script>".to_string();
    let mut host = TestHost::new(
      html,
      None,
      8,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();
 
    host.start(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
 
    assert_eq!(
      host.executor.log,
      vec!["script:a".to_string(), "microtask:a".to_string()],
      "import maps are not JavaScript and must not execute through the classic-script executor"
    );
    assert!(
      host.script_events.is_empty(),
      "inline import maps should not queue script load/error event tasks in this harness"
    );
    Ok(())
  }

  #[test]
  fn blocking_external_script_blocks_parsing_until_fetch_and_execute() -> Result<()> {
    let html = "<!doctype html><script src=a.js></script><script>b</script>".to_string();
    let mut host = TestHost::new(
      html,
      Some("https://example.com/dir/page.html"),
      16,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.executor.log, Vec::<String>::new());
    assert_eq!(host.fetcher.started.len(), 1);
    let (blocking_id, _, _, _) = host.fetcher.started[0].clone();
    assert_eq!(host.blocked_on, Some(blocking_id));

    host.queue_fetch_completed(blocking_id, "ext-a".to_string(), &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.blocked_on, None);
    assert_eq!(
      host.executor.log,
      vec![
        "script:ext-a".to_string(),
        "microtask:ext-a".to_string(),
        "script:b".to_string(),
        "microtask:b".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn async_scripts_execute_in_completion_order_and_can_run_during_parsing() -> Result<()> {
    let filler = "x".repeat(2048);
    let html = format!(
      "<!doctype html><script async src=a1.js></script><script async src=a2.js></script><p>{filler}</p><script>final</script>"
    );
    let mut host = TestHost::new(
      html,
      Some("https://example.com/dir/page.html"),
      32,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    spin_until_started_fetches(&mut host, &mut event_loop, 2)?;
    assert_eq!(host.fetcher.started.len(), 2);
    let a1 = host.fetcher.started[0].0;
    let a2 = host.fetcher.started[1].0;

    // Complete downloads out-of-order: a2 finishes before a1.
    host.queue_fetch_completed(a2, "a2".to_string(), &mut event_loop)?;
    host.queue_fetch_completed(a1, "a1".to_string(), &mut event_loop)?;

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    // Async scripts should execute in completion order and before the later inline script,
    // demonstrating that they can run while parsing is still in progress.
    assert_eq!(
      host.executor.log,
      vec![
        "script:a2".to_string(),
        "microtask:a2".to_string(),
        "script:a1".to_string(),
        "microtask:a1".to_string(),
        "script:final".to_string(),
        "microtask:final".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn load_waits_for_async_external_script_execution() -> Result<()> {
    use crate::web::dom::DocumentReadyState;
    let html = "<!doctype html><script async src=a.js></script>".to_string();
    let mut host = TestHost::new(
      html,
      Some("https://example.com/dir/page.html"),
      32,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    spin_until_started_fetches(&mut host, &mut event_loop, 1)?;
    let (script_id, _url, _dest, _credentials_mode) = host.fetcher.started[0].clone();

    // Allow parsing to finish and fire DOMContentLoaded, but do not complete the async script yet.
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let doc = host
      .finished_document
      .as_ref()
      .expect("document should have finished parsing");
    assert_eq!(
      doc.ready_state(),
      DocumentReadyState::Interactive,
      "expected DOMContentLoaded to have fired while async script is still pending"
    );
    assert!(
      host.pending_script_load_blockers.contains(&script_id),
      "expected async script to remain registered as a load blocker until executed"
    );
    assert_eq!(
      host.lifecycle_events,
      vec!["readystatechange".to_string(), "DOMContentLoaded".to_string()],
      "expected DOMContentLoaded but not load before async script execution"
    );

    // Now complete the async fetch and run the resulting tasks; `load` should fire afterwards.
    host.queue_fetch_completed(script_id, "A".to_string(), &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let doc = host
      .finished_document
      .as_ref()
      .expect("document should still exist");
    assert_eq!(doc.ready_state(), DocumentReadyState::Complete);
    assert!(
      !host.pending_script_load_blockers.contains(&script_id),
      "expected async script to complete its load blocker after execution"
    );
    assert_eq!(
      host.lifecycle_events,
      vec![
        "readystatechange".to_string(),
        "DOMContentLoaded".to_string(),
        "readystatechange".to_string(),
        "load".to_string(),
      ],
    );
    Ok(())
  }

  #[test]
  fn defer_scripts_execute_after_parsing_completed_in_document_order() -> Result<()> {
    let html = "<!doctype html><script defer src=d1.js></script><script defer src=d2.js></script>"
      .to_string();
    let mut host = TestHost::new(
      html,
      Some("https://example.com/dir/page.html"),
      16,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    spin_until_started_fetches(&mut host, &mut event_loop, 2)?;
    assert_eq!(host.fetcher.started.len(), 2);
    let d1 = host.fetcher.started[0].0;
    let d2 = host.fetcher.started[1].0;

    // Complete out-of-order.
    host.queue_fetch_completed(d2, "d2".to_string(), &mut event_loop)?;
    host.queue_fetch_completed(d1, "d1".to_string(), &mut event_loop)?;

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.executor.log,
      vec![
        "script:d1".to_string(),
        "microtask:d1".to_string(),
        "script:d2".to_string(),
        "microtask:d2".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn base_url_timing_is_honored_for_script_src_resolution() -> Result<()> {
    let html = r#"<!doctype html>
      <html>
        <head>
          <script async src="a.js"></script>
          <base href="https://ex/base/">
        </head>
        <body>
          <script async src="b.js"></script>
        </body>
      </html>"#
      .to_string();
    let mut host = TestHost::new(
      html,
      Some("https://example.com/dir/page.html"),
      64,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    let urls: Vec<&str> = host
      .fetcher
      .started
      .iter()
      .map(|(_, url, _, _)| url.as_str())
      .collect();
    assert_eq!(
      urls,
      vec!["https://example.com/dir/a.js", "https://ex/base/b.js"]
    );
    Ok(())
  }

  #[test]
  fn microtasks_run_before_parser_inserted_inline_script_boundary_even_inside_parse_task(
  ) -> Result<()> {
    let html = "<!doctype html><script>RUN</script>".to_string();
    // Use a large chunk size so the parser hits the </script> boundary within the first parsing
    // task. This reproduces the HTML requirement to perform a microtask checkpoint *mid-task*
    // (before preparing/executing the parser-inserted script).
    let mut host = TestHost::new(
      html,
      None,
      1024,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    // Queue a microtask *before* parsing begins. This must run before the parser-inserted script
    // executes, even though parsing runs inside a DOMManipulation task.
    event_loop.queue_microtask(|host, _event_loop| {
      host.executor.log.push("microtask".to_string());
      Ok(())
    })?;

    host.start(&mut event_loop)?;
    // Run the parse task first (without pre-draining microtasks) to ensure the pre-script checkpoint
    // at `</script>` boundaries is the mechanism that flushes the microtask.
    assert!(event_loop.run_next_task(&mut host)?);
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.executor.log,
      vec![
        "microtask".to_string(),
        "script:RUN".to_string(),
        "microtask:RUN".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn pre_script_microtask_checkpoint_is_skipped_when_js_execution_context_stack_nonempty(
  ) -> Result<()> {
    // Simulate re-entrant parsing (e.g. `document.write()` while a script is executing): the HTML
    // spec requires that the pre-script microtask checkpoint at `</script>` boundaries is skipped
    // when the JS execution context stack is not empty.
    let mut host = TestHost::new(
      String::new(),
      None,
      1,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    // Queue a microtask before encountering the script boundary. It must *not* run before the
    // script executes when we're already "in JS" (depth > 0).
    event_loop.queue_microtask(|host, _event_loop| {
      host.executor.log.push("microtask".to_string());
      Ok(())
    })?;

    // Feed the parser manually and pump until it hits the `</script>` boundary.
    host.parser.push_str("<!doctype html><script>RUN</script>");
    host.parser.set_eof();
    let script_node = match host.parser.pump()? {
      Html5everPump::Script(node) => node,
      Html5everPump::NeedMoreInput => panic!("expected pump to yield Script, got NeedMoreInput"),
      Html5everPump::Finished(_) => panic!("expected pump to yield Script, got Finished"),
    };

    // Simulate being inside a currently-executing script.
    {
      let _outer_js = host.enter_js_execution();
      host.handle_script_boundary(script_node, &mut event_loop)?;
      assert_eq!(host.executor.log, vec!["script:RUN".to_string()]);
    }

    // Once the outer script returns, the JS execution context stack becomes empty and the pending
    // microtasks can run.
    event_loop.perform_microtask_checkpoint(&mut host)?;

    assert_eq!(
      host.executor.log,
      vec![
        "script:RUN".to_string(),
        "microtask".to_string(),
        "microtask:RUN".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn skips_foreign_namespace_svg_scripts() -> Result<()> {
    // html5ever yields TokenizerResult::Script for SVG <script>, so the orchestrator must ensure it
    // does not execute it using HTML semantics.
    let html = "<!doctype html><svg xmlns=\"http://www.w3.org/2000/svg\"><script>bad</script></svg><script>good</script>"
      .to_string();
    let mut host = TestHost::new(
      html,
      None,
      32,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.executor.log,
      vec!["script:good".to_string(), "microtask:good".to_string(),]
    );
    Ok(())
  }

  #[test]
  fn classic_script_src_empty_queues_error_event_and_does_not_execute_inline() -> Result<()> {
    let html = "<!doctype html><script src=\"\">INLINE</script>".to_string();
    let mut host = TestHost::new(
      html,
      Some("https://example.com/dir/page.html"),
      32,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(host.fetcher.started.is_empty(), "invalid src must not start a fetch");
    assert_eq!(
      host.executor.log,
      Vec::<String>::new(),
      "presence of src must suppress inline execution even when src is empty"
    );
    assert_eq!(host.script_events.len(), 1);
    assert!(
      host.script_events[0].starts_with("error@"),
      "expected an error event task for invalid src"
    );
    Ok(())
  }

  #[test]
  fn classic_script_src_rejected_scheme_queues_error_event_and_does_not_execute_inline() -> Result<()> {
    let html = "<!doctype html><script src=\"javascript:alert(1)\">INLINE</script>".to_string();
    let mut host = TestHost::new(
      html,
      Some("https://example.com/dir/page.html"),
      32,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(host.fetcher.started.is_empty(), "invalid src must not start a fetch");
    assert!(
      host.executor.log.is_empty(),
      "presence of src must suppress inline execution even when src is rejected"
    );
    assert_eq!(host.script_events.len(), 1);
    assert!(host.script_events[0].starts_with("error@"));
    Ok(())
  }

  #[test]
  fn module_script_src_empty_queues_error_event_and_does_not_start_fetch() -> Result<()> {
    let html = "<!doctype html><script type=\"module\" src=\"\">INLINE</script>".to_string();
    let mut host = TestHost::new(
      html,
      Some("https://example.com/dir/page.html"),
      32,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    host.scheduler.set_options(crate::js::JsExecutionOptions {
      supports_module_scripts: true,
      ..Default::default()
    });
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(host.fetcher.started.is_empty(), "invalid module src must not start a fetch");
    assert!(
      host.executor.log.is_empty(),
      "module scripts are out-of-scope for execution; invalid src must not run inline"
    );
    assert_eq!(host.script_events.len(), 1);
    assert!(host.script_events[0].starts_with("error@"));
    Ok(())
  }

  #[test]
  fn module_script_src_rejected_scheme_queues_error_event_and_does_not_start_fetch() -> Result<()> {
    let html =
      "<!doctype html><script type=\"module\" src=\"javascript:alert(1)\">INLINE</script>".to_string();
    let mut host = TestHost::new(
      html,
      Some("https://example.com/dir/page.html"),
      32,
      ManualFetcher::default(),
      LoggingExecutor::default(),
    );
    host.scheduler.set_options(crate::js::JsExecutionOptions {
      supports_module_scripts: true,
      ..Default::default()
    });
    let mut event_loop = EventLoop::<TestHost>::new();

    host.start(&mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(
      host.fetcher.started.is_empty(),
      "invalid module src must not start a fetch"
    );
    assert!(host.executor.log.is_empty());
    assert_eq!(host.script_events.len(), 1);
    assert!(host.script_events[0].starts_with("error@"));
    Ok(())
  }
}
