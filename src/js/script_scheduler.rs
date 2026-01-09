use crate::error::{Error, RenderStage, Result};
use crate::render_control::{record_stage, StageGuard, StageHeartbeat};
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

use super::{EventLoop, JsExecutionOptions, ScriptElementSpec, ScriptType, TaskSource};

/// A minimal script loader interface used by the script scheduler.
///
/// This trait is intentionally tiny so the scheduler can be unit-tested without integrating with
/// FastRender's real networking/resource pipeline yet.
pub trait ScriptLoader {
  /// An opaque identifier returned by `start_load`.
  type Handle: Copy + Eq + Hash + Debug;

  /// Load the script resource synchronously.
  ///
  /// Used for parser-blocking external scripts.
  fn load_blocking(&mut self, url: &str) -> Result<String>;

  /// Start loading the script resource in a non-blocking way.
  ///
  /// Used for async and defer scripts.
  fn start_load(&mut self, url: &str) -> Result<Self::Handle>;

  /// Poll for the next completed non-blocking script load.
  ///
  /// Implementations should return completions in the order they completed.
  fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>>;
}

/// A minimal script executor interface used by the script scheduler.
///
/// The real implementation will be the JS engine integration (ecma-rs + web bindings). Keeping it
/// behind a trait lets us deterministically test scheduling and microtask checkpoint behavior.
pub trait ScriptExecutor: Sized {
  /// Execute the provided classic script source text.
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()>;
}

struct DeferredScript {
  spec: Option<ScriptElementSpec>,
  source: Option<String>,
}

/// A classic `<script>` scheduler implementing the HTML Standard ordering model:
/// parser-blocking vs async vs defer.
///
/// - **parser-blocking**: inline scripts, and external scripts without `async`/`defer`
/// - **async**: external scripts with `async` (also used for non-parser-inserted external scripts)
/// - **defer**: external parser-inserted scripts with `defer` and not `async`
///
/// Async and deferred scripts are executed as event loop tasks (`TaskSource::Script`), so the event
/// loop's "microtasks after tasks" rule naturally applies.
///
/// Parser-blocking scripts execute synchronously (during parsing) and explicitly perform a
/// microtask checkpoint after execution, per HTML.
pub struct ClassicScriptScheduler<Host>
where
  Host: ScriptLoader + ScriptExecutor,
{
  options: JsExecutionOptions,
  parsing_finished: bool,

  async_pending: HashMap<<Host as ScriptLoader>::Handle, ScriptElementSpec>,

  defer_scripts: Vec<DeferredScript>,
  defer_by_handle: HashMap<<Host as ScriptLoader>::Handle, usize>,
  next_defer_to_queue: usize,
}

impl<Host> Default for ClassicScriptScheduler<Host>
where
  Host: ScriptLoader + ScriptExecutor,
{
  fn default() -> Self {
    Self {
      options: JsExecutionOptions::default(),
      parsing_finished: false,
      async_pending: HashMap::new(),
      defer_scripts: Vec::new(),
      defer_by_handle: HashMap::new(),
      next_defer_to_queue: 0,
    }
  }
}

impl<Host> ClassicScriptScheduler<Host>
where
  Host: ScriptLoader + ScriptExecutor,
{
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_options(options: JsExecutionOptions) -> Self {
    Self { options, ..Self::default() }
  }

  pub fn options(&self) -> JsExecutionOptions {
    self.options
  }

  /// Handle a `<script>` element encountered during parsing / insertion.
  pub fn handle_script(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    spec: ScriptElementSpec,
  ) -> Result<()> {
    // Only classic scripts are scheduled/executed by this scheduler for now.
    if spec.script_type != ScriptType::Classic {
      return Ok(());
    }

    // Inline scripts execute immediately (async/defer ignored).
    if spec.src.is_none() {
      self
        .options
        .check_script_source(&spec.inline_text, "source=inline")?;
      // HTML `</script>` handling performs a microtask checkpoint *before* preparing/executing a
      // parser-inserted script when the JS execution context stack is empty.
      //
      // In this MVP we approximate "JS execution context stack is empty" by checking whether the
      // event loop is currently executing a task/microtask. This prevents microtasks from running
      // during re-entrant parsing (e.g. `document.write()` inside an executing script).
      if spec.parser_inserted && event_loop.currently_running_task().is_none() {
        event_loop.perform_microtask_checkpoint(host)?;
      }
      {
        let _stage_guard = StageGuard::install(Some(RenderStage::Script));
        record_stage(StageHeartbeat::Script);
        host.execute_classic_script(&spec.inline_text, &spec, event_loop)?;
      }
      // HTML: a microtask checkpoint is performed after script execution.
      event_loop.perform_microtask_checkpoint(host)?;
      return Ok(());
    }

    // External script.
    let Some(src_url) = spec.src.as_deref() else {
      return Err(Error::Other(
        "internal error: external script spec missing src URL".to_string(),
      ));
    };

    // Async takes priority over defer. Also: non-parser-inserted external scripts are (roughly)
    // async by default; defer is only meaningful for parser-inserted scripts.
    if spec.async_attr || !spec.parser_inserted {
      let handle = host.start_load(src_url)?;
      if self.async_pending.contains_key(&handle) || self.defer_by_handle.contains_key(&handle) {
        return Err(Error::Other(format!(
          "Script loader returned a duplicate handle: {handle:?}"
        )));
      }
      self.async_pending.insert(handle, spec);
      return Ok(());
    }

    if spec.defer_attr {
      let handle = host.start_load(src_url)?;
      if self.async_pending.contains_key(&handle) || self.defer_by_handle.contains_key(&handle) {
        return Err(Error::Other(format!(
          "Script loader returned a duplicate handle: {handle:?}"
        )));
      }
      let idx = self.defer_scripts.len();
      self.defer_scripts.push(DeferredScript {
        spec: Some(spec),
        source: None,
      });
      self.defer_by_handle.insert(handle, idx);
      return Ok(());
    }

    // Parser-blocking external script: synchronously load + execute.
    if spec.parser_inserted && event_loop.currently_running_task().is_none() {
      event_loop.perform_microtask_checkpoint(host)?;
    }
    let script_text = host.load_blocking(src_url)?;
    self
      .options
      .check_script_source(&script_text, &format!("source=external url={src_url}"))?;
    {
      let _stage_guard = StageGuard::install(Some(RenderStage::Script));
      record_stage(StageHeartbeat::Script);
      host.execute_classic_script(&script_text, &spec, event_loop)?;
    }
    event_loop.perform_microtask_checkpoint(host)?;
    Ok(())
  }

  /// Poll pending async/defer loads and schedule any newly-completed scripts.
  pub fn poll(&mut self, host: &mut Host, event_loop: &mut EventLoop<Host>) -> Result<()> {
    while let Some((handle, source)) = host.poll_complete()? {
      self
        .options
        .check_script_source(&source, &format!("source=external handle={handle:?}"))?;
      if let Some(spec) = self.async_pending.remove(&handle) {
        Self::queue_script_task(event_loop, spec, source)?;
        continue;
      }
      if let Some(idx) = self.defer_by_handle.remove(&handle) {
        let entry = self.defer_scripts.get_mut(idx).ok_or_else(|| {
          Error::Other(format!(
            "internal error: defer_by_handle index out of bounds (idx={idx})"
          ))
        })?;
        entry.source = Some(source);
        continue;
      }
      return Err(Error::Other(format!(
        "Script loader returned completion for unknown handle: {handle:?}"
      )));
    }

    self.queue_ready_deferred(event_loop)?;
    Ok(())
  }

  /// Hook for "parsing finished" to allow deferred scripts to run.
  pub fn finish_parsing(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
  ) -> Result<()> {
    self.parsing_finished = true;
    self.poll(host, event_loop)
  }

  fn queue_ready_deferred(&mut self, event_loop: &mut EventLoop<Host>) -> Result<()> {
    if !self.parsing_finished {
      return Ok(());
    }

    while self.next_defer_to_queue < self.defer_scripts.len() {
      let entry = &mut self.defer_scripts[self.next_defer_to_queue];
      let Some(source) = entry.source.take() else {
        break;
      };
      self
        .options
        .check_script_source(&source, &format!("source=external defer_idx={}", self.next_defer_to_queue))?;
      let spec = entry
        .spec
        .take()
        .ok_or_else(|| Error::Other("internal error: deferred script missing spec".to_string()))?;
      self.next_defer_to_queue += 1;
      Self::queue_script_task(event_loop, spec, source)?;
    }
    Ok(())
  }

  fn queue_script_task(
    event_loop: &mut EventLoop<Host>,
    spec: ScriptElementSpec,
    source: String,
  ) -> Result<()> {
    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
      host.execute_classic_script(&source, &spec, event_loop)
    })?;
    Ok(())
  }
}

/// A deterministic, event-driven scheduler for classic, parser-inserted `<script>` elements.
///
/// This models a subset of the HTML Standard script processing model:
/// - Inline classic scripts execute immediately and block parsing.
/// - External classic scripts:
///   - no `async`/`defer`: parsing-blocking (execute immediately on fetch completion).
///   - `defer`: execute after parsing completes, in document order.
///   - `async`: execute ASAP on fetch completion, not ordered.
/// - A microtask checkpoint after each script execution (performed by the orchestrator).
///
/// Out of scope (intentionally not modeled here):
/// - Module scripts (`type="module"`) and import maps.
/// - CSP, `nomodule`, stylesheet-blocking scripts, `document.write`, and dynamic insertion.
///
/// ## Orchestrator contract
///
/// The scheduler does not execute scripts itself. Callers drive it with explicit events and perform
/// returned actions:
/// - For [`ScriptSchedulerAction::ExecuteNow`], execute the script synchronously and then call
///   [`EventLoop::perform_microtask_checkpoint`] (HTML microtask checkpoint after script execution).
/// - For [`ScriptSchedulerAction::QueueTask`], enqueue a task with [`TaskSource::Script`]; the event
///   loop's "microtasks after tasks" rule provides the checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScriptId(u64);

impl ScriptId {
  pub fn as_u64(self) -> u64 {
    self.0
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredScript<NodeId> {
  pub id: ScriptId,
  pub actions: Vec<ScriptSchedulerAction<NodeId>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptSchedulerAction<NodeId> {
  /// Begin fetching an external script.
  StartFetch {
    script_id: ScriptId,
    node_id: NodeId,
    url: String,
  },
  /// Block the HTML parser until the referenced script has executed.
  ///
  /// This is emitted for parsing-blocking external scripts (no `async`/`defer`).
  BlockParserUntilExecuted { script_id: ScriptId, node_id: NodeId },
  /// Execute a script immediately (synchronously in the caller's stack).
  ///
  /// The orchestrator must perform a microtask checkpoint immediately after executing the script.
  ExecuteNow {
    script_id: ScriptId,
    node_id: NodeId,
    source_text: String,
  },
  /// Queue script execution as an event-loop task.
  ///
  /// The event loop performs a microtask checkpoint after each task, which satisfies the HTML
  /// microtask checkpoint requirement after script execution.
  QueueTask {
    script_id: ScriptId,
    node_id: NodeId,
    source_text: String,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalMode {
  Blocking,
  Defer,
  Async,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalScriptEntry<NodeId> {
  #[allow(dead_code)]
  node_id: NodeId,
  #[allow(dead_code)]
  base_url_at_discovery: Option<String>,
  #[allow(dead_code)]
  url: String,
  mode: ExternalMode,
  fetch_completed: bool,
  source_text: Option<String>,
  queued_for_execution: bool,
}

pub struct ScriptScheduler<NodeId> {
  next_script_id: u64,
  scripts: HashMap<ScriptId, ExternalScriptEntry<NodeId>>,
  defer_queue: Vec<ScriptId>,
  next_defer_to_queue: usize,
  parsing_completed: bool,
}

impl<NodeId: Copy> Default for ScriptScheduler<NodeId> {
  fn default() -> Self {
    Self {
      next_script_id: 1,
      scripts: HashMap::new(),
      defer_queue: Vec::new(),
      next_defer_to_queue: 0,
      parsing_completed: false,
    }
  }
}

impl<NodeId: Copy> ScriptScheduler<NodeId> {
  pub fn new() -> Self {
    Self::default()
  }

  fn alloc_script_id(&mut self) -> ScriptId {
    let id = ScriptId(self.next_script_id);
    self.next_script_id += 1;
    id
  }

  /// Notify the scheduler that the HTML parser has discovered a parser-inserted `<script>`.
  pub fn discovered_parser_script(
    &mut self,
    element: ScriptElementSpec,
    node_id: NodeId,
    base_url_at_discovery: Option<String>,
  ) -> Result<DiscoveredScript<NodeId>> {
    let id = self.alloc_script_id();

    if element.script_type != ScriptType::Classic {
      // Non-classic scripts are out-of-scope for this scheduler.
      return Ok(DiscoveredScript {
        id,
        actions: Vec::new(),
      });
    }

    let mut actions: Vec<ScriptSchedulerAction<NodeId>> = Vec::new();

    let src = element.src.filter(|s| !s.is_empty());
    if let Some(url) = src {
      let mode = if element.async_attr {
        ExternalMode::Async
      } else if element.defer_attr {
        ExternalMode::Defer
      } else {
        ExternalMode::Blocking
      };

      if mode == ExternalMode::Defer {
        self.defer_queue.push(id);
      }

      self.scripts.insert(
        id,
        ExternalScriptEntry {
          node_id,
          base_url_at_discovery,
          url: url.clone(),
          mode,
          fetch_completed: false,
          source_text: None,
          queued_for_execution: false,
        },
      );

      actions.push(ScriptSchedulerAction::StartFetch {
        script_id: id,
        node_id,
        url,
      });

      if mode == ExternalMode::Blocking {
        actions.push(ScriptSchedulerAction::BlockParserUntilExecuted { script_id: id, node_id });
      }
    } else {
      // Inline classic scripts execute immediately and block parsing.
      actions.push(ScriptSchedulerAction::ExecuteNow {
        script_id: id,
        node_id,
        source_text: element.inline_text,
      });
    }

    Ok(DiscoveredScript { id, actions })
  }

  /// Notify the scheduler that a previously requested external script fetch completed.
  pub fn fetch_completed(
    &mut self,
    script_id: ScriptId,
    source_text: String,
  ) -> Result<Vec<ScriptSchedulerAction<NodeId>>> {
    let Some(entry) = self.scripts.get_mut(&script_id) else {
      return Err(Error::Other(format!(
        "fetch_completed called for unknown script_id={}",
        script_id.as_u64()
      )));
    };

    if entry.fetch_completed {
      return Err(Error::Other(format!(
        "fetch_completed called more than once for script_id={}",
        script_id.as_u64()
      )));
    }
    entry.fetch_completed = true;
    entry.source_text = Some(source_text);

    match entry.mode {
      ExternalMode::Blocking => {
        if entry.queued_for_execution {
          return Ok(Vec::new());
        }
        entry.queued_for_execution = true;
        let node_id = entry.node_id;
        let source_text = entry.source_text.take().ok_or_else(|| {
          Error::Other("internal error: missing source text after fetch_completed".to_string())
        })?;
        Ok(vec![ScriptSchedulerAction::ExecuteNow {
          script_id,
          node_id,
          source_text,
        }])
      }
      ExternalMode::Async => {
        if entry.queued_for_execution {
          return Ok(Vec::new());
        }
        entry.queued_for_execution = true;
        let node_id = entry.node_id;
        let source_text = entry.source_text.take().ok_or_else(|| {
          Error::Other("internal error: missing source text after fetch_completed".to_string())
        })?;
        Ok(vec![ScriptSchedulerAction::QueueTask {
          script_id,
          node_id,
          source_text,
        }])
      }
      ExternalMode::Defer => self.queue_defer_scripts_if_ready(),
    }
  }

  /// Notify the scheduler that HTML parsing has completed.
  pub fn parsing_completed(&mut self) -> Result<Vec<ScriptSchedulerAction<NodeId>>> {
    self.parsing_completed = true;
    self.queue_defer_scripts_if_ready()
  }

  fn queue_defer_scripts_if_ready(&mut self) -> Result<Vec<ScriptSchedulerAction<NodeId>>> {
    if !self.parsing_completed {
      return Ok(Vec::new());
    }

    let mut actions: Vec<ScriptSchedulerAction<NodeId>> = Vec::new();
    while self.next_defer_to_queue < self.defer_queue.len() {
      let script_id = self.defer_queue[self.next_defer_to_queue];
      let Some(entry) = self.scripts.get_mut(&script_id) else {
        return Err(Error::Other(format!(
          "internal error: defer_queue references missing script_id={}",
          script_id.as_u64()
        )));
      };

      if entry.queued_for_execution {
        self.next_defer_to_queue += 1;
        continue;
      }

      if entry.source_text.is_none() {
        break;
      }

      entry.queued_for_execution = true;
      let node_id = entry.node_id;
      let source_text = entry.source_text.take().ok_or_else(|| {
        Error::Other(
          "internal error: missing source text when queueing deferred scripts".to_string(),
        )
      })?;
      actions.push(ScriptSchedulerAction::QueueTask {
        script_id,
        node_id,
        source_text,
      });
      self.next_defer_to_queue += 1;
    }

    Ok(actions)
  }
}

#[cfg(test)]
mod tests {
  #![allow(deprecated)]

  use super::*;
  use crate::dom::parse_html;
  use crate::js::extract_script_elements;
  use crate::js::{EventLoop, JsExecutionOptions, RunLimits, ScriptElementSpec, ScriptType};
  use std::collections::VecDeque;

  #[derive(Default)]
  struct ManualLoader {
    next_handle: usize,
    handles_by_url: HashMap<String, usize>,
    completed: VecDeque<(usize, String)>,
    blocking_sources: HashMap<String, String>,
  }

  impl ManualLoader {
    fn complete_url(&mut self, url: &str, source: &str) {
      let handle = *self
        .handles_by_url
        .get(url)
        .unwrap_or_else(|| panic!("no pending load for url={url}"));
      self.completed.push_back((handle, source.to_string()));
    }
  }

  #[derive(Default)]
  struct TestHost {
    loader: ManualLoader,
    log: Vec<String>,
    queue_microtask_after_execute: bool,
  }

  impl TestHost {
    fn new(queue_microtask_after_execute: bool) -> Self {
      Self {
        loader: ManualLoader::default(),
        log: Vec::new(),
        queue_microtask_after_execute,
      }
    }
  }

  impl ScriptLoader for TestHost {
    type Handle = usize;

    fn load_blocking(&mut self, url: &str) -> Result<String> {
      self
        .loader
        .blocking_sources
        .get(url)
        .cloned()
        .ok_or_else(|| crate::error::Error::Other(format!("no blocking source for url={url}")))
    }

    fn start_load(&mut self, url: &str) -> Result<Self::Handle> {
      let handle = self.loader.next_handle;
      self.loader.next_handle += 1;
      self.loader.handles_by_url.insert(url.to_string(), handle);
      Ok(handle)
    }

    fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>> {
      Ok(self.loader.completed.pop_front())
    }
  }

  impl ScriptExecutor for TestHost {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self.log.push(script_text.to_string());
      if self.queue_microtask_after_execute {
        let name = script_text.to_string();
        event_loop.queue_microtask(move |host, _event_loop| {
          host.log.push(format!("microtask-after-{name}"));
          Ok(())
        })?;
      }
      Ok(())
    }
  }

  fn inline_script(text: &str) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: None,
      inline_text: text.to_string(),
      async_attr: false,
      defer_attr: false,
      parser_inserted: true,
      script_type: ScriptType::Classic,
    }
  }

  fn external_script(url: &str, async_attr: bool, defer_attr: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: Some(url.to_string()),
      inline_text: String::new(),
      async_attr,
      defer_attr,
      parser_inserted: true,
      script_type: ScriptType::Classic,
    }
  }

  #[test]
  fn blocking_inline_scripts_run_immediately() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    scheduler.handle_script(&mut host, &mut event_loop, inline_script("A"))?;
    scheduler.handle_script(&mut host, &mut event_loop, inline_script("B"))?;

    assert_eq!(host.log, vec!["A".to_string(), "B".to_string()]);
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.log, vec!["A".to_string(), "B".to_string()]);
    Ok(())
  }

  #[test]
  fn external_defer_scripts_execute_after_parsing_complete_in_order() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script("d1", false, true),
    )?;
    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script("d2", false, true),
    )?;

    // Complete downloads out-of-order: d2 finishes before d1.
    host.loader.complete_url("d2", "d2");
    host.loader.complete_url("d1", "d1");

    scheduler.finish_parsing(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["d1".to_string(), "d2".to_string()]);
    Ok(())
  }

  #[test]
  fn external_async_scripts_execute_in_completion_order() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script("a1", true, false),
    )?;
    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script("a2", true, false),
    )?;

    // Complete downloads out-of-order: a2 finishes before a1.
    host.loader.complete_url("a2", "a2");
    host.loader.complete_url("a1", "a1");

    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["a2".to_string(), "a1".to_string()]);
    Ok(())
  }

  #[test]
  fn microtask_checkpoint_runs_after_every_script() -> Result<()> {
    let mut host = TestHost::new(true);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    // Parser-blocking inline scripts should perform an explicit microtask checkpoint before
    // parsing resumes.
    scheduler.handle_script(&mut host, &mut event_loop, inline_script("A"))?;
    assert_eq!(
      host.log,
      vec!["A".to_string(), "microtask-after-A".to_string()]
    );

    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script("a1", true, false),
    )?;
    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script("a2", true, false),
    )?;
    host.loader.complete_url("a1", "a1");
    host.loader.complete_url("a2", "a2");

    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.log,
      vec![
        "A".to_string(),
        "microtask-after-A".to_string(),
        "a1".to_string(),
        "microtask-after-a1".to_string(),
        "a2".to_string(),
        "microtask-after-a2".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn microtasks_run_before_parser_blocking_inline_script_at_script_end_boundary() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask".to_string());
      Ok(())
    })?;

    let dom = parse_html("<!doctype html><script>BLOCK</script>")?;
    let scripts = extract_script_elements(&dom, None);
    assert_eq!(scripts.len(), 1);
    assert_eq!(scripts[0].inline_text, "BLOCK");

    scheduler.handle_script(&mut host, &mut event_loop, scripts[0].clone())?;

    assert_eq!(host.log, vec!["microtask".to_string(), "BLOCK".to_string()]);
    Ok(())
  }

  #[test]
  fn rejects_inline_scripts_larger_than_max_script_bytes() {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let options = JsExecutionOptions {
      max_script_bytes: 3,
      ..JsExecutionOptions::default()
    };
    let mut scheduler = ClassicScriptScheduler::<TestHost>::with_options(options);

    let err = scheduler
      .handle_script(&mut host, &mut event_loop, inline_script("ABCD"))
      .expect_err("expected oversized script to be rejected");
    assert!(matches!(err, Error::Other(msg) if msg.contains("max_script_bytes")));
    assert_eq!(host.log, Vec::<String>::new());
  }
}

#[cfg(test)]
mod state_machine_tests {
  use super::*;
  use crate::js::{EventLoop, RunLimits, TaskSource};

  #[derive(Default)]
  struct Host {
    log: Vec<String>,
  }

  fn execute_fake_script(
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    source_text: &str,
  ) -> Result<()> {
    host.log.push(format!("script:{source_text}"));
    let micro = format!("microtask:{source_text}");
    event_loop.queue_microtask(move |host, _| {
      host.log.push(micro);
      Ok(())
    })?;
    Ok(())
  }

  fn classic_inline(text: &str) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: None,
      inline_text: text.to_string(),
      async_attr: false,
      defer_attr: false,
      parser_inserted: true,
      script_type: ScriptType::Classic,
    }
  }

  fn classic_external(src: &str, async_attr: bool, defer_attr: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: Some(src.to_string()),
      inline_text: String::new(),
      async_attr,
      defer_attr,
      parser_inserted: true,
      script_type: ScriptType::Classic,
    }
  }

  struct Harness {
    scheduler: ScriptScheduler<u32>,
    event_loop: EventLoop<Host>,
    host: Host,
    started_fetches: Vec<(ScriptId, u32, String)>,
    blocked_parser_on: Option<ScriptId>,
  }

  impl Harness {
    fn new() -> Self {
      Self {
        scheduler: ScriptScheduler::new(),
        event_loop: EventLoop::new(),
        host: Host::default(),
        started_fetches: Vec::new(),
        blocked_parser_on: None,
      }
    }

    fn apply_actions(&mut self, actions: Vec<ScriptSchedulerAction<u32>>) -> Result<()> {
      for action in actions {
        match action {
          ScriptSchedulerAction::StartFetch {
            script_id,
            node_id,
            url,
          } => {
            self.started_fetches.push((script_id, node_id, url));
          }
          ScriptSchedulerAction::BlockParserUntilExecuted { script_id, node_id: _ } => {
            self.blocked_parser_on = Some(script_id);
          }
          ScriptSchedulerAction::ExecuteNow {
            script_id,
            node_id: _,
            source_text,
          } => {
            execute_fake_script(&mut self.host, &mut self.event_loop, &source_text)?;
            self
              .event_loop
              .perform_microtask_checkpoint(&mut self.host)?;
            if self.blocked_parser_on == Some(script_id) {
              self.blocked_parser_on = None;
            }
          }
          ScriptSchedulerAction::QueueTask {
            script_id: _,
            node_id: _,
            source_text,
          } => {
            self
              .event_loop
              .queue_task(TaskSource::Script, move |host, event_loop| {
                execute_fake_script(host, event_loop, &source_text)
              })?;
          }
        }
      }
      Ok(())
    }

    fn discover(&mut self, element: ScriptElementSpec) -> Result<ScriptId> {
      let discovered = self.scheduler.discovered_parser_script(
        element, /* node_id */ 1, /* base_url_at_discovery */ None,
      )?;
      let id = discovered.id;
      self.apply_actions(discovered.actions)?;
      Ok(id)
    }

    fn fetch_complete(&mut self, script_id: ScriptId, source_text: &str) -> Result<()> {
      let actions = self
        .scheduler
        .fetch_completed(script_id, source_text.to_string())?;
      self.apply_actions(actions)
    }

    fn parsing_completed(&mut self) -> Result<()> {
      let actions = self.scheduler.parsing_completed()?;
      self.apply_actions(actions)
    }

    fn run_event_loop(&mut self) -> Result<()> {
      self
        .event_loop
        .run_until_idle(&mut self.host, RunLimits::unbounded())?;
      Ok(())
    }
  }

  #[test]
  fn inline_scripts_execute_in_order_and_flush_microtasks_between() -> Result<()> {
    let mut h = Harness::new();

    h.discover(classic_inline("a"))?;
    h.discover(classic_inline("b"))?;

    assert_eq!(
      h.host.log,
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
  fn blocking_external_script_delays_later_inline_script_until_fetch_and_execute() -> Result<()> {
    let mut h = Harness::new();

    let blocking_id = h.discover(classic_external("https://example.com/a.js", false, false))?;
    assert_eq!(h.blocked_parser_on, Some(blocking_id));
    assert_eq!(
      h.started_fetches,
      vec![(blocking_id, 1u32, "https://example.com/a.js".to_string())]
    );

    // Parser cannot progress to the next `<script>` until the blocking script fetch completes and
    // executes.
    h.fetch_complete(blocking_id, "ext-a")?;
    assert_eq!(h.blocked_parser_on, None);

    h.discover(classic_inline("b"))?;

    assert_eq!(
      h.host.log,
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
  fn defer_scripts_run_after_parsing_completed_in_document_order() -> Result<()> {
    let mut h = Harness::new();

    let d1 = h.discover(classic_external("https://example.com/d1.js", false, true))?;
    let d2 = h.discover(classic_external("https://example.com/d2.js", false, true))?;

    // Fetch completion order is irrelevant for `defer`.
    h.fetch_complete(d2, "d2")?;
    h.parsing_completed()?;
    // D1 isn't ready yet, so nothing should have run.
    h.run_event_loop()?;
    assert_eq!(h.host.log, Vec::<String>::new());

    h.fetch_complete(d1, "d1")?;
    h.run_event_loop()?;

    assert_eq!(
      h.host.log,
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
  fn async_scripts_run_in_completion_order_and_can_interleave_with_defer() -> Result<()> {
    let mut h = Harness::new();

    let d1 = h.discover(classic_external("https://example.com/d1.js", false, true))?;
    let a1 = h.discover(classic_external("https://example.com/a1.js", true, false))?;
    let d2 = h.discover(classic_external("https://example.com/d2.js", false, true))?;
    let a2 = h.discover(classic_external("https://example.com/a2.js", true, false))?;

    // D1 becomes ready before parsing completes, but defer scripts cannot run yet.
    h.fetch_complete(d1, "d1")?;

    // Once parsing completes, the first defer script is queued.
    h.parsing_completed()?;

    // Async scripts execute in completion order, and can run between defer scripts.
    h.fetch_complete(a2, "a2")?;
    h.fetch_complete(d2, "d2")?;
    h.fetch_complete(a1, "a1")?;

    h.run_event_loop()?;

    assert_eq!(
      h.host.log,
      vec![
        "script:d1".to_string(),
        "microtask:d1".to_string(),
        "script:a2".to_string(),
        "microtask:a2".to_string(),
        "script:d2".to_string(),
        "microtask:d2".to_string(),
        "script:a1".to_string(),
        "microtask:a1".to_string(),
      ]
    );
    Ok(())
  }
}
