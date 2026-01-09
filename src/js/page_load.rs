use crate::dom2::{Document, Dom2TreeSink, NodeId};
use crate::error::Result;
use crate::html::pausable_html5ever::{Html5everPump, PausableHtml5everParser};
use crate::js::{EventLoop, ScriptElementSpec, ScriptScheduler, ScriptSchedulerAction, TaskSource};

use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::ParseOpts;
use std::collections::HashMap;

/// Script fetch adapter used by [`HtmlLoadOrchestrator`].
///
/// For now this is an extremely small surface: the orchestrator issues start-fetch requests for
/// external scripts and unit tests drive completion via
/// [`HtmlLoadOrchestrator::queue_fetch_completed`].
pub trait ScriptFetcher {
  fn start_fetch(&mut self, script_id: crate::js::ScriptId, url: &str) -> Result<()>;
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

    match self.parser.pump() {
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

  fn handle_script_boundary(&mut self, script_node: NodeId, event_loop: &mut EventLoop<Self>) -> Result<()> {
    let spec = self.build_script_spec(script_node);
    let base_url_at_discovery = self.parser.sink().current_base_url();
    let discovered = self.scheduler.discovered_parser_script(
      spec,
      script_node,
      base_url_at_discovery,
    )?;
    self.script_nodes.insert(discovered.id, script_node);
    self.apply_actions(discovered.actions, event_loop)?;
    Ok(())
  }

  fn build_script_spec(&self, script_node: NodeId) -> ScriptElementSpec {
    let sink = self.parser.sink();
    let doc = sink.document();
    let base = sink.base_url_tracker();
    crate::js::streaming::build_parser_inserted_script_element_spec_dom2(&doc, script_node, &base)
  }

  fn apply_actions(
    &mut self,
    actions: Vec<ScriptSchedulerAction<NodeId>>,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    for action in actions {
      match action {
        ScriptSchedulerAction::StartFetch { script_id, url, .. } => {
          self.fetcher.start_fetch(script_id, &url)?;
        }
        ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
          self.blocked_on = Some(script_id);
        }
        ScriptSchedulerAction::ExecuteNow {
          script_id,
          source_text,
          ..
        } => {
          {
            let executor = &mut self.executor;
            executor.execute(&source_text, event_loop)?;
          }
          event_loop.perform_microtask_checkpoint(self)?;
          if self.blocked_on == Some(script_id) {
            self.blocked_on = None;
            self.queue_parse_task(event_loop)?;
          }
        }
        ScriptSchedulerAction::QueueTask {
          script_id: _,
          source_text,
          ..
        } => {
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            host.executor.execute(&source_text, event_loop)
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
    started: Vec<(crate::js::ScriptId, String)>,
  }

  impl ScriptFetcher for ManualFetcher {
    fn start_fetch(&mut self, script_id: crate::js::ScriptId, url: &str) -> Result<()> {
      self.started.push((script_id, url.to_string()));
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
    let mut host = TestHost::new(html, None, 8, ManualFetcher::default(), LoggingExecutor::default());
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
    let (blocking_id, _) = host.fetcher.started[0].clone();
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
  fn defer_scripts_execute_after_parsing_completed_in_document_order() -> Result<()> {
    let html = "<!doctype html><script defer src=d1.js></script><script defer src=d2.js></script>".to_string();
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
      .map(|(_, url)| url.as_str())
      .collect();
    assert_eq!(
      urls,
      vec!["https://example.com/dir/a.js", "https://ex/base/b.js"]
    );
    Ok(())
  }
}
