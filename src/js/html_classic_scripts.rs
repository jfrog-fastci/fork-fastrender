use crate::dom2::{Document, NodeId};
use crate::error::{Error, Result};
use crate::js::dom_host::DomHost;
use crate::js::orchestrator::{CurrentScriptHost, CurrentScriptStateHandle};
use crate::js::script_scheduler::ScriptId;
use crate::js::streaming_pipeline::{ClassicScriptPipeline, ClassicScriptPipelineHost, ParseBudget};
use crate::js::{EventLoop, ScriptType};
use crate::resource::{FetchedResource, FetchContextKind, ResourceFetcher};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Fetch interface used by [`parse_and_run_classic_scripts`].
///
/// This is a deliberately tiny API: the parser/scheduler orchestrator requests a fetch via
/// [`ClassicScriptFetcher::start_fetch`] and the caller is expected to provide deterministic
/// completions via [`ClassicScriptFetcher::poll_complete`].
pub trait ClassicScriptFetcher {
  /// Start fetching an external script URL.
  fn start_fetch(&mut self, script_id: ScriptId, url: &str) -> Result<()>;

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

  fn fetch_text(&self, url: &str) -> Result<String> {
    let res: FetchedResource = self
      .fetcher
      .fetch_with_context(FetchContextKind::Other, url)?;
    Ok(String::from_utf8_lossy(&res.bytes).into_owned())
  }
}

impl ClassicScriptFetcher for ResourceFetcherClassicScriptFetcher {
  fn start_fetch(&mut self, script_id: ScriptId, url: &str) -> Result<()> {
    let text = self.fetch_text(url)?;
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
  fn start_fetch(&mut self, script_id: ScriptId, url: &str) -> Result<()> {
    if self.in_flight_fetches.contains_key(&script_id) {
      return Err(Error::Other(format!(
        "duplicate script fetch request (script_id={})",
        script_id.as_u64()
      )));
    }
    self.in_flight_fetches.insert(script_id, url.to_string());
    self.fetcher.start_fetch(script_id, url)
  }

  fn execute_script(
    &mut self,
    source_text: &str,
    script_node_id: NodeId,
    script_type: ScriptType,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let mut exec = self
      .executor
      .take()
      .expect("ClassicScriptRunner executor missing");
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
/// - the action-based [`ScriptScheduler`] classic-script model (`async` / `defer` / blocking)
/// - [`EventLoop`] tasks + microtasks (microtask checkpoint after scripts)
/// - [`ScriptOrchestrator`] `Document.currentScript` bookkeeping (via [`CurrentScriptHost`])
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
      let doc = pipeline
        .finished_document()
        .expect("parsing_finished implies finished_document");
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
