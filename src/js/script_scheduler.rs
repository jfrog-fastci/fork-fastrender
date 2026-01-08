use crate::error::Result;
use std::collections::HashMap;
use std::hash::Hash;
use std::fmt::Debug;

use super::{EventLoop, ScriptElementSpec, ScriptType, TaskSource};

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
      host.execute_classic_script(&spec.inline_text, &spec, event_loop)?;
      // HTML: a microtask checkpoint is performed after script execution.
      event_loop.perform_microtask_checkpoint(host)?;
      return Ok(());
    }

    // External script.
    let src_url = spec
      .src
      .as_deref()
      .expect("src was checked to be Some above");

    // Async takes priority over defer. Also: non-parser-inserted external scripts are (roughly)
    // async by default; defer is only meaningful for parser-inserted scripts.
    if spec.async_attr || !spec.parser_inserted {
      let handle = host.start_load(src_url)?;
      let prev = self.async_pending.insert(handle, spec);
      debug_assert!(prev.is_none(), "script loader returned a duplicate handle");
      return Ok(());
    }

    if spec.defer_attr {
      let handle = host.start_load(src_url)?;
      let idx = self.defer_scripts.len();
      self.defer_scripts.push(DeferredScript {
        spec: Some(spec),
        source: None,
      });
      let prev = self.defer_by_handle.insert(handle, idx);
      debug_assert!(prev.is_none(), "script loader returned a duplicate handle");
      return Ok(());
    }

    // Parser-blocking external script: synchronously load + execute.
    let script_text = host.load_blocking(src_url)?;
    host.execute_classic_script(&script_text, &spec, event_loop)?;
    event_loop.perform_microtask_checkpoint(host)?;
    Ok(())
  }

  /// Poll pending async/defer loads and schedule any newly-completed scripts.
  pub fn poll(&mut self, host: &mut Host, event_loop: &mut EventLoop<Host>) -> Result<()> {
    while let Some((handle, source)) = host.poll_complete()? {
      if let Some(spec) = self.async_pending.remove(&handle) {
        Self::queue_script_task(event_loop, spec, source)?;
        continue;
      }
      if let Some(idx) = self.defer_by_handle.remove(&handle) {
        let entry = self
          .defer_scripts
          .get_mut(idx)
          .expect("defer_by_handle indices must refer to existing entries");
        entry.source = Some(source);
        continue;
      }
    }

    self.queue_ready_deferred(event_loop)?;
    Ok(())
  }

  /// Hook for "parsing finished" to allow deferred scripts to run.
  pub fn finish_parsing(&mut self, host: &mut Host, event_loop: &mut EventLoop<Host>) -> Result<()> {
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
      let spec = entry
        .spec
        .take()
        .expect("deferred script should have a spec until it is queued");
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::{EventLoop, RunLimits, ScriptElementSpec, ScriptType};
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

    scheduler.handle_script(&mut host, &mut event_loop, external_script("d1", false, true))?;
    scheduler.handle_script(&mut host, &mut event_loop, external_script("d2", false, true))?;

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

    scheduler.handle_script(&mut host, &mut event_loop, external_script("a1", true, false))?;
    scheduler.handle_script(&mut host, &mut event_loop, external_script("a2", true, false))?;

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

    scheduler.handle_script(&mut host, &mut event_loop, external_script("a1", true, false))?;
    scheduler.handle_script(&mut host, &mut event_loop, external_script("a2", true, false))?;
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
}
