use crate::dom2::{Document, NodeId};
use crate::error::{Error, Result};
use crate::js::dom_host::DomHost;
use crate::js::orchestrator::{CurrentScriptHost, CurrentScriptStateHandle};
use crate::js::script_encoding::decode_classic_script_bytes;
use crate::js::script_scheduler::ScriptId;
use crate::js::streaming_pipeline::{
  ClassicScriptPipeline, ClassicScriptPipelineHost, ParseBudget,
};
use crate::js::{EventLoop, ScriptType};
use crate::resource::{
  ensure_script_mime_sane, FetchCredentialsMode, FetchDestination, FetchRequest, FetchedResource,
  ResourceFetcher,
};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Fetch interface used by [`parse_and_run_classic_scripts`].
///
/// This is a deliberately tiny API: the parser/scheduler orchestrator requests a fetch via
/// [`ClassicScriptFetcher::start_fetch`] and the caller is expected to provide deterministic
/// completions via [`ClassicScriptFetcher::poll_complete`].
pub trait ClassicScriptFetcher {
  /// Start fetching an external script URL.
  fn start_fetch(
    &mut self,
    script_id: ScriptId,
    url: &str,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
  ) -> Result<()>;

  /// Poll the next completed fetch, returning `(script_id, source_text)`.
  ///
  /// Implementations should return completions in the order they completed.
  fn poll_complete(&mut self) -> Result<Option<(ScriptId, String)>>;
}

/// Adapter that loads scripts using FastRender's real [`ResourceFetcher`].
///
/// This is a synchronous implementation: `start_fetch` performs the fetch immediately and stores
/// the completion to be returned by `poll_complete`.
#[derive(Clone)]
pub struct ResourceFetcherClassicScriptFetcher {
  fetcher: Arc<dyn ResourceFetcher>,
  completed: VecDeque<(ScriptId, String)>,
}

impl ResourceFetcherClassicScriptFetcher {
  pub fn new(fetcher: Arc<dyn ResourceFetcher>) -> Self {
    Self {
      fetcher,
      completed: VecDeque::new(),
    }
  }

  fn fetch_text(
    &self,
    url: &str,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
  ) -> Result<String> {
    let res: FetchedResource = self.fetcher.fetch_with_request(
      FetchRequest::new(url, destination).with_credentials_mode(credentials_mode),
    )?;
    ensure_script_mime_sane(&res, url)?;
    Ok(decode_classic_script_bytes(
      &res.bytes,
      res.content_type.as_deref(),
      encoding_rs::UTF_8,
    ))
  }
}

impl ClassicScriptFetcher for ResourceFetcherClassicScriptFetcher {
  fn start_fetch(
    &mut self,
    script_id: ScriptId,
    url: &str,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
  ) -> Result<()> {
    let text = self.fetch_text(url, destination, credentials_mode)?;
    self.completed.push_back((script_id, text));
    Ok(())
  }

  fn poll_complete(&mut self) -> Result<Option<(ScriptId, String)>> {
    Ok(self.completed.pop_front())
  }
}

/// JavaScript execution hook used by [`parse_and_run_classic_scripts`].
pub trait ClassicScriptExecutor<Host>
where
  Host: CurrentScriptHost + DomHost + 'static,
{
  fn execute(
    &mut self,
    host: &mut Host,
    source_text: &str,
    script_node_id: NodeId,
    script_type: ScriptType,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()>;
}

struct ClassicScriptRunner<F, E> {
  dom: Document,
  current_script: CurrentScriptStateHandle,
  fetcher: F,
  executor: Option<E>,
  in_flight_fetches: HashMap<ScriptId, String>,
}

impl<F, E> DomHost for ClassicScriptRunner<F, E> {
  fn with_dom<R, Func>(&self, f: Func) -> R
  where
    Func: FnOnce(&Document) -> R,
  {
    f(&self.dom)
  }

  fn mutate_dom<R, Func>(&mut self, f: Func) -> R
  where
    Func: FnOnce(&mut Document) -> (R, bool),
  {
    let (result, _changed) = f(&mut self.dom);
    result
  }
}

impl<F, E> CurrentScriptHost for ClassicScriptRunner<F, E> {
  fn current_script_state(&self) -> &CurrentScriptStateHandle {
    &self.current_script
  }
}

impl<F, E> ClassicScriptPipelineHost for ClassicScriptRunner<F, E>
where
  F: ClassicScriptFetcher + 'static,
  E: ClassicScriptExecutor<Self> + 'static,
{
  fn start_fetch(
    &mut self,
    script_id: ScriptId,
    url: &str,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
  ) -> Result<()> {
    if self.in_flight_fetches.contains_key(&script_id) {
      return Err(Error::Other(format!(
        "duplicate script fetch request (script_id={})",
        script_id.as_u64()
      )));
    }
    self.in_flight_fetches.insert(script_id, url.to_string());
    self
      .fetcher
      .start_fetch(script_id, url, destination, credentials_mode)
  }

  fn execute_script(
    &mut self,
    source_text: &str,
    script_node_id: NodeId,
    script_type: ScriptType,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let mut exec = match self.executor.take() {
      Some(exec) => exec,
      None => {
        return Err(Error::Other(
          "ClassicScriptRunner executor missing".to_string(),
        ))
      }
    };
    let result = exec.execute(self, source_text, script_node_id, script_type, event_loop);
    self.executor = Some(exec);
    result
  }
}

/// Parse an HTML document and execute parser-inserted classic scripts following the HTML Standard
/// classic script processing model (v1 subset).
///
/// This is a synchronous, deterministic harness intended for unit/integration tests and early
/// plumbing (it is not a production browser pipeline). It wires together:
/// - the streaming HTML parser (`StreamingHtmlParser`) via [`ClassicScriptPipeline`]
/// - parse-time base URL timing (`<base href>` resolution at script discovery time)
/// - the action-based [`crate::js::script_scheduler::ScriptScheduler`] classic-script model (`async` / `defer` / blocking)
/// - [`EventLoop`] tasks + microtasks (microtask checkpoint after scripts)
/// - [`crate::js::orchestrator::ScriptOrchestrator`] `Document.currentScript` bookkeeping (via [`CurrentScriptHost`])
pub fn parse_and_run_classic_scripts<F, E>(
  html: &str,
  document_url: Option<&str>,
  fetcher: F,
  executor: E,
) -> Result<Document>
where
  F: ClassicScriptFetcher + 'static,
  E: ClassicScriptExecutor<ClassicScriptRunner<F, E>> + 'static,
{
  // Small parse budget so async scripts can interleave with parsing in deterministic tests.
  let mut pipeline = ClassicScriptPipeline::<ClassicScriptRunner<F, E>>::new_with_parse_budget(
    document_url,
    ParseBudget::new(1),
  );

  let mut host = ClassicScriptRunner {
    dom: Document::new(selectors::context::QuirksMode::NoQuirks),
    current_script: CurrentScriptStateHandle::default(),
    fetcher,
    executor: Some(executor),
    in_flight_fetches: HashMap::new(),
  };

  // Feed the document incrementally to ensure the parser yields (so async scripts can "interrupt"
  // parsing via the event loop).
  let chunk_size = 1024usize;
  let mut cursor = 0usize;
  let mut eof_sent = false;

  let mut tasks_executed: usize = 0;
  let max_tasks: usize = 1_000_000;

  loop {
    // 1) Deliver any completed fetches as networking tasks.
    let mut delivered_any_fetch = false;
    while let Some((script_id, source_text)) = host.fetcher.poll_complete()? {
      let Some(url) = host.in_flight_fetches.remove(&script_id) else {
        return Err(Error::Other(format!(
          "fetch completion received for unknown script_id={}",
          script_id.as_u64()
        )));
      };
      let _ = url; // currently only used for diagnostics above; keep for future logging/hooks.
      pipeline.queue_fetch_completion(script_id, source_text)?;
      delivered_any_fetch = true;
    }

    // 2) Run a single task (tasks are followed by microtask checkpoints by EventLoop).
    if pipeline.event_loop().run_next_task(&mut host)? {
      tasks_executed += 1;
      if tasks_executed > max_tasks {
        return Err(Error::Other(format!(
          "parse_and_run_classic_scripts exceeded max task budget (limit={max_tasks})"
        )));
      }
      continue;
    }

    // 3) If we just delivered fetch completions, new tasks should exist; loop to run them.
    if delivered_any_fetch {
      continue;
    }

    // 4) No tasks and no new fetch completions: decide whether to feed more input or finish.
    if pipeline.parsing_finished() {
      if !host.in_flight_fetches.is_empty() {
        return Err(Error::Other(format!(
          "HTML parsing finished but {} external script fetches never completed",
          host.in_flight_fetches.len()
        )));
      }
      let Some(doc) = pipeline.finished_document() else {
        return Err(Error::Other(
          "HTML parsing finished but no document was produced".to_string(),
        ));
      };
      host.dom = doc.clone();
      return Ok(doc);
    }

    if pipeline.blocked_on_script().is_some() {
      // Parser is blocked on a parser-blocking external script fetch, but the fetcher reported no
      // completion and there is no more queued work. This is a deadlock in the test harness /
      // fetcher.
      let pending = host
        .in_flight_fetches
        .keys()
        .map(|id| id.as_u64().to_string())
        .collect::<Vec<_>>()
        .join(", ");
      return Err(Error::Other(format!(
        "HTML parsing is blocked on an external script fetch, but the fetcher produced no completion (pending_script_ids=[{pending}])"
      )));
    }

    // Parser is waiting for more input (`NeedMoreInput`).
    if cursor < html.len() {
      let mut end = (cursor + chunk_size).min(html.len());
      while end > cursor && !html.is_char_boundary(end) {
        end -= 1;
      }
      if end == cursor {
        // Ensure forward progress even if `chunk_size` splits a multi-byte character.
        if let Some(ch) = html[cursor..].chars().next() {
          end = (cursor + ch.len_utf8()).min(html.len());
        }
      }

      let chunk = &html[cursor..end];
      cursor = end;
      pipeline.feed_str(chunk)?;
      continue;
    }

    if !eof_sent {
      pipeline.finish_input()?;
      eof_sent = true;
      continue;
    }

    return Err(Error::Other(
      "classic script pipeline became idle before parsing completed".to_string(),
    ));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::cell::RefCell;
  use std::collections::{HashMap, VecDeque};
  use std::rc::Rc;

  #[derive(Default)]
  struct FakeFetcher {
    sources: HashMap<String, String>,
    completion_plan: VecDeque<String>,
    started_by_url: HashMap<String, ScriptId>,
    started_urls: Rc<RefCell<Vec<String>>>,
    ready: VecDeque<(ScriptId, String)>,
  }

  impl FakeFetcher {
    fn new() -> Self {
      Self::default()
    }

    fn with_source(mut self, url: &str, source: &str) -> Self {
      self.sources.insert(url.to_string(), source.to_string());
      self
    }

    fn with_completion_order(mut self, urls: &[&str]) -> Self {
      self.completion_plan = urls.iter().map(|u| u.to_string()).collect();
      self
    }
  }

  impl ClassicScriptFetcher for FakeFetcher {
    fn start_fetch(
      &mut self,
      script_id: ScriptId,
      url: &str,
      destination: FetchDestination,
      credentials_mode: FetchCredentialsMode,
    ) -> Result<()> {
      assert_eq!(
        destination,
        FetchDestination::Script,
        "classic <script src> fetches should use FetchDestination::Script"
      );
      assert_eq!(
        credentials_mode,
        FetchCredentialsMode::Include,
        "classic <script src> fetches should default to credentials=include"
      );
      self.started_urls.borrow_mut().push(url.to_string());
      self.started_by_url.insert(url.to_string(), script_id);
      Ok(())
    }

    fn poll_complete(&mut self) -> Result<Option<(ScriptId, String)>> {
      // Promote ready fetches in the configured completion order, but only once the URL has been
      // started (i.e. discovered by the parser).
      while let Some(next_url) = self.completion_plan.front().cloned() {
        let Some(&script_id) = self.started_by_url.get(&next_url) else {
          break;
        };
        self.completion_plan.pop_front();
        let source = self
          .sources
          .get(&next_url)
          .cloned()
          .ok_or_else(|| Error::Other(format!("no script source for url={next_url}")))?;
        self.ready.push_back((script_id, source));
      }
      Ok(self.ready.pop_front())
    }
  }

  #[derive(Default, Clone)]
  struct RecordingExecutor {
    events: Rc<RefCell<Vec<String>>>,
    current_during_exec: Rc<RefCell<Vec<(crate::dom2::NodeId, Option<crate::dom2::NodeId>)>>>,
    current_during_microtask: Rc<RefCell<Vec<Option<crate::dom2::NodeId>>>>,
  }

  impl RecordingExecutor {
    fn new() -> Self {
      Self::default()
    }
  }

  impl<Host> ClassicScriptExecutor<Host> for RecordingExecutor
  where
    Host: CurrentScriptHost + DomHost + 'static,
  {
    fn execute(
      &mut self,
      host: &mut Host,
      source_text: &str,
      script_node_id: crate::dom2::NodeId,
      script_type: ScriptType,
      event_loop: &mut EventLoop<Host>,
    ) -> Result<()> {
      assert_eq!(
        script_type,
        ScriptType::Classic,
        "parse_and_run_classic_scripts should execute only classic scripts in v1"
      );
      self
        .events
        .borrow_mut()
        .push(format!("script:{source_text}"));
      self
        .current_during_exec
        .borrow_mut()
        .push((script_node_id, host.current_script()));

      let events = Rc::clone(&self.events);
      let current = Rc::clone(&self.current_during_microtask);
      let text = source_text.to_string();
      event_loop.queue_microtask(move |host, _event_loop| {
        events.borrow_mut().push(format!("microtask:{text}"));
        current.borrow_mut().push(host.current_script());
        Ok(())
      })?;

      Ok(())
    }
  }

  #[test]
  fn blocking_external_script_delays_later_inline_script_until_fetched_and_executed() -> Result<()> {
    let html = "<!doctype html><script src=\"https://ex/a.js\"></script><script>INLINE</script>";

    let exec = RecordingExecutor::new();
    let events = Rc::clone(&exec.events);

    let fetcher = FakeFetcher::new()
      .with_source("https://ex/a.js", "A")
      .with_completion_order(&["https://ex/a.js"]);

    let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

    assert_eq!(
      &*events.borrow(),
      &[
        "script:A".to_string(),
        "microtask:A".to_string(),
        "script:INLINE".to_string(),
        "microtask:INLINE".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn importmap_is_ignored_by_classic_script_runner() -> Result<()> {
    // `parse_and_run_classic_scripts` is intentionally classic-only; `type="importmap"` must be
    // ignored when module scripts are disabled.
    let html = concat!(
      "<!doctype html>",
      "<script type=\"importmap\">{\"imports\":{}}</script>",
      "<script>INLINE</script>",
    );

    let exec = RecordingExecutor::new();
    let events = Rc::clone(&exec.events);

    let fetcher = FakeFetcher::new();

    let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

    assert_eq!(
      &*events.borrow(),
      &["script:INLINE".to_string(), "microtask:INLINE".to_string()]
    );
    Ok(())
  }

  #[test]
  fn async_external_scripts_execute_in_completion_order() -> Result<()> {
    let html = concat!(
      "<!doctype html>",
      "<script async src=\"https://ex/a1.js\"></script>",
      "<script async src=\"https://ex/a2.js\"></script>",
    );

    let exec = RecordingExecutor::new();
    let events = Rc::clone(&exec.events);

    let fetcher = FakeFetcher::new()
      .with_source("https://ex/a1.js", "a1")
      .with_source("https://ex/a2.js", "a2")
      // a2 completes before a1, regardless of discovery order.
      .with_completion_order(&["https://ex/a2.js", "https://ex/a1.js"]);

    let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

    assert_eq!(
      &*events.borrow(),
      &[
        "script:a2".to_string(),
        "microtask:a2".to_string(),
        "script:a1".to_string(),
        "microtask:a1".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn defer_scripts_execute_after_parsing_completes_in_document_order() -> Result<()> {
    let html = concat!(
      "<!doctype html>",
      "<script defer src=\"https://ex/d1.js\"></script>",
      "<script defer src=\"https://ex/d2.js\"></script>",
      "<div id=end></div>",
    );

    let exec = RecordingExecutor::new();
    let events = Rc::clone(&exec.events);

    let fetcher = FakeFetcher::new()
      .with_source("https://ex/d1.js", "d1")
      .with_source("https://ex/d2.js", "d2")
      // d2 completes before d1.
      .with_completion_order(&["https://ex/d2.js", "https://ex/d1.js"]);

    let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

    assert_eq!(
      &*events.borrow(),
      &[
        "script:d1".to_string(),
        "microtask:d1".to_string(),
        "script:d2".to_string(),
        "microtask:d2".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn microtask_checkpoint_runs_after_every_script_and_clears_current_script() -> Result<()> {
    let html = concat!(
      "<!doctype html>",
      "<script>ONE</script>",
      "<script async src=\"https://ex/a.js\"></script>",
      "<script>THREE</script>",
    );

    let exec = RecordingExecutor::new();
    let events = Rc::clone(&exec.events);
    let current_exec = Rc::clone(&exec.current_during_exec);
    let current_micro = Rc::clone(&exec.current_during_microtask);

    let fetcher = FakeFetcher::new()
      .with_source("https://ex/a.js", "TWO")
      .with_completion_order(&["https://ex/a.js"]);

    let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

    // Async scripts can run at any point after they are discovered and downloaded, so only assert:
    // - scripts/microtasks alternate (microtask checkpoint runs after each script),
    // - inline scripts preserve document order relative to each other.
    let ev = events.borrow();
    assert_eq!(ev.len(), 6, "expected 3 scripts + 3 microtask checkpoints");
    for pair in ev.chunks(2) {
      let [script, micro] = pair else {
        unreachable!()
      };
      assert!(
        script.starts_with("script:"),
        "expected script entry, got {script:?}"
      );
      assert_eq!(
        micro,
        &script.replacen("script:", "microtask:", 1),
        "expected microtask checkpoint entry following {script:?}"
      );
    }
    let idx_one = ev.iter().position(|e| e == "script:ONE").unwrap();
    let idx_three = ev.iter().position(|e| e == "script:THREE").unwrap();
    assert!(
      idx_one < idx_three,
      "expected inline scripts to execute in document order"
    );

    // `Document.currentScript` should be set during each classic script execution.
    assert_eq!(current_exec.borrow().len(), 3);
    for (script_node_id, observed_current) in current_exec.borrow().iter().copied() {
      assert_eq!(
        observed_current,
        Some(script_node_id),
        "expected currentScript to be set to the executing script element"
      );
    }
    // It should be cleared/restored by the time microtasks run after the script.
    assert_eq!(current_micro.borrow().len(), 3);
    assert!(current_micro.borrow().iter().all(|v| v.is_none()));
    Ok(())
  }

  #[test]
  fn base_url_is_snapshotted_at_script_discovery_time() -> Result<()> {
    let html = r#"<!doctype html>
      <html><head>
        <script src="a.js"></script>
        <base href="https://ex/base/">
      </head></html>"#;

    let started_urls = Rc::new(RefCell::new(Vec::new()));
    let mut fetcher = FakeFetcher::new()
      .with_source("https://ex/a.js", "A")
      .with_completion_order(&["https://ex/a.js"]);
    fetcher.started_urls = Rc::clone(&started_urls);

    let exec = RecordingExecutor::new();
    let _doc = parse_and_run_classic_scripts(html, Some("https://ex/doc.html"), fetcher, exec)?;

    assert_eq!(
      &*started_urls.borrow(),
      &["https://ex/a.js".to_string()],
      "expected <script src=\"a.js\"> before <base> to resolve against the document URL"
    );
    Ok(())
  }
}
