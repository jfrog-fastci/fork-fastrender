use crate::error::{Error, RenderStage, Result};
use crate::render_control::{record_stage, StageGuard, StageHeartbeat};
use crate::resource::{FetchCredentialsMode, FetchDestination};
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::rc::Rc;

use super::{EventLoop, JsExecutionOptions, ScriptElementSpec, ScriptType, TaskSource};

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

fn enter_js_execution(depth: &Rc<Cell<usize>>) -> JsExecutionGuard {
  let cur = depth.get();
  depth.set(cur + 1);
  JsExecutionGuard {
    depth: Rc::clone(depth),
  }
}

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
  fn load_blocking(
    &mut self,
    url: &str,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
  ) -> Result<String>;

  /// Start loading the script resource in a non-blocking way.
  ///
  /// Used for async and defer scripts.
  fn start_load(
    &mut self,
    url: &str,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
  ) -> Result<Self::Handle>;

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

  /// Execute the provided module script source text (`type="module"`).
  ///
  /// Note: module scripts must never execute synchronously in the parser/DOM mutation call stack.
  /// Scheduling is responsible for ensuring this runs from a queued task.
  fn execute_module_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()>;
}

/// Host hook for firing `<script>` element `load`/`error` events.
///
/// HTML queues these as element tasks on the DOM manipulation task source.
pub trait ScriptEventDispatcher: Sized {
  fn dispatch_script_event(&mut self, event: ScriptElementEvent, spec: &ScriptElementSpec)
    -> Result<()>;
}

struct DeferredScript {
  spec: Option<ScriptElementSpec>,
  source: Option<String>,
}

struct InOrderAsapScript {
  spec: Option<ScriptElementSpec>,
  source: Option<String>,
}

/// An HTML `<script>` scheduler implementing a subset of the HTML Standard ordering model for
/// classic and module scripts.
///
/// - **parser-blocking**: inline scripts, and external scripts without `async`/`defer`
/// - **async**: external scripts with `async`, or with the HTML "force async" flag set
/// - **in-order-asap**: external scripts that are not parser-inserted and are not async-like
/// - **defer**: external parser-inserted scripts with `defer` and not `async`
///
/// Module scripts (`type="module"`):
/// - Module scripts never execute synchronously in the parser/DOM mutation call stack; they always
///   execute from a queued task.
/// - Parser-inserted module scripts are deferred-by-default:
///   - `async` present: execute ASAP once ready (may run before parsing completes).
///   - otherwise: execute after parsing completes (the `defer` attribute has no effect).
/// - Non-parser-inserted module scripts:
///   - `async` present: execute ASAP once ready.
///   - otherwise: execute in insertion order as soon as possible once ready.
///
/// Async and deferred scripts are executed as event loop tasks (`TaskSource::Script`), so the event
/// loop's "microtasks after tasks" rule naturally applies.
///
/// Parser-blocking scripts execute synchronously (during parsing) and explicitly perform a
/// microtask checkpoint after execution, per HTML.
pub struct ClassicScriptScheduler<Host>
where
  Host: ScriptLoader + ScriptExecutor + ScriptEventDispatcher,
{
  options: JsExecutionOptions,
  js_execution_depth: Rc<Cell<usize>>,
  parsing_finished: bool,

  async_pending: HashMap<<Host as ScriptLoader>::Handle, ScriptElementSpec>,
  module_async_pending: HashMap<<Host as ScriptLoader>::Handle, ScriptElementSpec>,

  in_order_asap_scripts: Vec<InOrderAsapScript>,
  in_order_asap_by_handle: HashMap<<Host as ScriptLoader>::Handle, usize>,
  next_in_order_asap_to_queue: usize,

  defer_scripts: Vec<DeferredScript>,
  defer_by_handle: HashMap<<Host as ScriptLoader>::Handle, usize>,
  next_defer_to_queue: usize,

  module_in_order_scripts: Vec<InOrderAsapScript>,
  module_in_order_by_handle: HashMap<<Host as ScriptLoader>::Handle, usize>,
  next_module_in_order_to_queue: usize,
}

impl<Host> Default for ClassicScriptScheduler<Host>
where
  Host: ScriptLoader + ScriptExecutor + ScriptEventDispatcher,
{
  fn default() -> Self {
    Self {
      options: JsExecutionOptions::default(),
      js_execution_depth: Rc::new(Cell::new(0)),
      parsing_finished: false,
      async_pending: HashMap::new(),
      module_async_pending: HashMap::new(),
      in_order_asap_scripts: Vec::new(),
      in_order_asap_by_handle: HashMap::new(),
      next_in_order_asap_to_queue: 0,
      defer_scripts: Vec::new(),
      defer_by_handle: HashMap::new(),
      next_defer_to_queue: 0,
      module_in_order_scripts: Vec::new(),
      module_in_order_by_handle: HashMap::new(),
      next_module_in_order_to_queue: 0,
    }
  }
}

impl<Host> ClassicScriptScheduler<Host>
where
  Host: ScriptLoader + ScriptExecutor + ScriptEventDispatcher,
{
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_options(options: JsExecutionOptions) -> Self {
    Self {
      options,
      ..Self::default()
    }
  }

  pub fn options(&self) -> JsExecutionOptions {
    self.options
  }

  /// Returns a handle to the shared "JS execution context stack depth" counter used by this
  /// scheduler.
  ///
  /// Embeddings that execute JS outside the scheduler (e.g., timers, host-initiated scripts, or
  /// dynamic `<script>` insertion during JS execution) can use this handle to participate in the
  /// same depth tracking so microtask checkpoints occur only when the JS stack is empty.
  pub fn js_execution_depth_handle(&self) -> Rc<Cell<usize>> {
    Rc::clone(&self.js_execution_depth)
  }

  /// Handle a `<script>` element encountered during parsing / insertion.
  pub fn handle_script(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    spec: ScriptElementSpec,
  ) -> Result<()> {
    match spec.script_type {
      ScriptType::Classic => {}
      ScriptType::Module => {
        if !self.options.supports_module_scripts {
          // HTML: unsupported module scripts are ignored.
          return Ok(());
        }
        return self.handle_module_script(host, event_loop, spec);
      }
      // Import maps and unknown script types are ignored by FastRender's execution pipeline.
      ScriptType::ImportMap | ScriptType::Unknown => return Ok(()),
    }
    // HTML: When a user agent supports module scripts, classic scripts with the `nomodule`
    // attribute must be ignored completely (not fetched/executed).
    if spec.is_suppressed_by_nomodule(&self.options) {
      return Ok(());
    }

    // Inline scripts execute immediately (async/defer ignored).
    //
    // Note: the HTML script processing model treats the *presence* of the `src` attribute as
    // suppressing inline execution, even if the value is empty/invalid. Therefore we key off
    // `src_attr_present` instead of `src.is_none()`.
    if !spec.src_attr_present {
      // HTML: if the script has no `src` attribute and the source text is empty, preparing the
      // script is a no-op and the element must remain eligible to run after later mutation.
      if spec.parser_inserted && self.js_execution_depth.get() == 0 {
        event_loop.perform_microtask_checkpoint(host)?;
      }
      if spec.inline_text.is_empty() {
        return Ok(());
      }
      self
        .options
        .check_script_source(&spec.inline_text, "source=inline")?;
      {
        let _stage_guard = StageGuard::install(Some(RenderStage::Script));
        record_stage(StageHeartbeat::Script);
        {
          let _guard = enter_js_execution(&self.js_execution_depth);
          host.execute_classic_script(&spec.inline_text, &spec, event_loop)?;
        }
      }
      // HTML: "clean up after running script" performs a microtask checkpoint only when the JS
      // execution context stack is empty. Nested (re-entrant) script execution must not drain
      // microtasks until the outermost script returns.
      if self.js_execution_depth.get() == 0 {
        event_loop.perform_microtask_checkpoint(host)?;
      }
      return Ok(());
    }

    // External script.
    let Some(src_url) = spec.src.as_deref() else {
      // `src` attribute present but empty/invalid/unresolvable: per HTML this fires an error event
      // and does not fall back to inline execution.
      event_loop.queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
        host.dispatch_script_event(ScriptElementEvent::Error, &spec)
      })?;
      return Ok(());
    };
    // Classic scripts are fetched in `no-cors` mode by default. When the HTML "CORS settings
    // attribute" (`crossorigin`) is present, scripts are fetched in `cors` mode and the credentials
    // mode follows the attribute value.
    let (destination, credentials_mode) = if let Some(cors_mode) = spec.crossorigin {
      (FetchDestination::ScriptCors, cors_mode.credentials_mode())
    } else {
      (FetchDestination::Script, FetchCredentialsMode::Include)
    };

    // Async takes priority over defer. For classic scripts, HTML treats `async` as true when either
    // the `async` attribute is present or the per-element "force async" flag is set.
    let async_like = spec.async_attr || spec.force_async;

    if async_like {
      let handle = host.start_load(src_url, destination, credentials_mode)?;
      if self.async_pending.contains_key(&handle)
        || self.module_async_pending.contains_key(&handle)
        || self.defer_by_handle.contains_key(&handle)
        || self.in_order_asap_by_handle.contains_key(&handle)
        || self.module_in_order_by_handle.contains_key(&handle)
      {
        return Err(Error::Other(format!(
          "Script loader returned a duplicate handle: {handle:?}"
        )));
      }
      self.async_pending.insert(handle, spec);
      return Ok(());
    }

    if !spec.parser_inserted {
      let handle = host.start_load(src_url, destination, credentials_mode)?;
      if self.async_pending.contains_key(&handle)
        || self.module_async_pending.contains_key(&handle)
        || self.defer_by_handle.contains_key(&handle)
        || self.in_order_asap_by_handle.contains_key(&handle)
        || self.module_in_order_by_handle.contains_key(&handle)
      {
        return Err(Error::Other(format!(
          "Script loader returned a duplicate handle: {handle:?}"
        )));
      }
      let idx = self.in_order_asap_scripts.len();
      self.in_order_asap_scripts.push(InOrderAsapScript {
        spec: Some(spec),
        source: None,
      });
      self.in_order_asap_by_handle.insert(handle, idx);
      return Ok(());
    }

    if spec.defer_attr {
      let handle = host.start_load(src_url, destination, credentials_mode)?;
      if self.async_pending.contains_key(&handle)
        || self.module_async_pending.contains_key(&handle)
        || self.defer_by_handle.contains_key(&handle)
        || self.in_order_asap_by_handle.contains_key(&handle)
        || self.module_in_order_by_handle.contains_key(&handle)
      {
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
    if spec.parser_inserted && self.js_execution_depth.get() == 0 {
      event_loop.perform_microtask_checkpoint(host)?;
    }
    let script_text = host.load_blocking(src_url, destination, credentials_mode)?;
    self
      .options
      .check_script_source(&script_text, &format!("source=external url={src_url}"))?;
    {
      let _stage_guard = StageGuard::install(Some(RenderStage::Script));
      record_stage(StageHeartbeat::Script);
      {
        let _guard = enter_js_execution(&self.js_execution_depth);
        host.execute_classic_script(&script_text, &spec, event_loop)?;
      }
    }
    // HTML: external scripts fire a `load` event once they have executed.
    if spec.src_attr_present {
      let spec_for_event = spec.clone();
      event_loop.queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
        host.dispatch_script_event(ScriptElementEvent::Load, &spec_for_event)
      })?;
    }
    if self.js_execution_depth.get() == 0 {
      event_loop.perform_microtask_checkpoint(host)?;
    }
    Ok(())
  }

  fn handle_module_script(
    &mut self,
    host: &mut Host,
    event_loop: &mut EventLoop<Host>,
    mut spec: ScriptElementSpec,
  ) -> Result<()> {
    // Module scripts never execute synchronously inside the parser/DOM mutation call stack. The
    // HTML Standard queues module script execution as tasks, and `defer` is ignored.

    // Inline module script.
    // Note: `src_attr_present` suppresses inline execution even if the URL is empty/invalid.
    if !spec.src_attr_present {
      let source = std::mem::take(&mut spec.inline_text);
      self.options.check_script_source(&source, "source=inline")?;

      if spec.async_attr {
        // Async module scripts execute ASAP once ready.
        self.queue_script_task(event_loop, spec, source)?;
        return Ok(());
      }

      if spec.parser_inserted {
        // Parser-inserted module scripts without `async` execute after parsing completes (defer-like).
        self
          .defer_scripts
          .push(DeferredScript { spec: Some(spec), source: Some(source) });
        self.queue_ready_deferred(event_loop)?;
        return Ok(());
      }

      // Dynamic module scripts without `async` execute in insertion order as soon as possible.
      self
        .module_in_order_scripts
        .push(InOrderAsapScript { spec: Some(spec), source: Some(source) });
      self.queue_ready_module_in_order(event_loop)?;
      return Ok(());
    }

    // External module script.
    let Some(src_url) = spec.src.as_deref() else {
      // `src` attribute present but empty/invalid/unresolvable: per HTML this fires an error event
      // and does not fall back to inline execution.
      event_loop.queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
        host.dispatch_script_event(ScriptElementEvent::Error, &spec)
      })?;
      return Ok(());
    };
    // Module scripts are fetched in `cors` mode and default to `same-origin` credentials. The
    // `crossorigin` attribute only affects the credentials mode.
    let destination = FetchDestination::ScriptCors;
    let credentials_mode = spec
      .crossorigin
      .map(|cors_mode| cors_mode.credentials_mode())
      .unwrap_or(FetchCredentialsMode::SameOrigin);

    // External module scripts start fetching as early as possible. Unlike classic scripts, dynamic
    // module scripts are not async-by-default: the absence of `async` means they execute in order.
    let handle = host.start_load(src_url, destination, credentials_mode)?;
    if self.async_pending.contains_key(&handle)
      || self.module_async_pending.contains_key(&handle)
      || self.defer_by_handle.contains_key(&handle)
      || self.in_order_asap_by_handle.contains_key(&handle)
      || self.module_in_order_by_handle.contains_key(&handle)
    {
      return Err(Error::Other(format!(
        "Script loader returned a duplicate handle: {handle:?}"
      )));
    }

    if spec.async_attr {
      self.module_async_pending.insert(handle, spec);
      return Ok(());
    }

    if spec.parser_inserted {
      let idx = self.defer_scripts.len();
      self.defer_scripts.push(DeferredScript { spec: Some(spec), source: None });
      self.defer_by_handle.insert(handle, idx);
      return Ok(());
    }

    let idx = self.module_in_order_scripts.len();
    self
      .module_in_order_scripts
      .push(InOrderAsapScript { spec: Some(spec), source: None });
    self.module_in_order_by_handle.insert(handle, idx);
    Ok(())
  }

  /// Poll pending async/defer loads and schedule any newly-completed scripts.
  pub fn poll(&mut self, host: &mut Host, event_loop: &mut EventLoop<Host>) -> Result<()> {
    while let Some((handle, source)) = host.poll_complete()? {
      self
        .options
        .check_script_source(&source, &format!("source=external handle={handle:?}"))?;
      if let Some(spec) = self.async_pending.remove(&handle) {
        self.queue_script_task(event_loop, spec, source)?;
        continue;
      }
      if let Some(spec) = self.module_async_pending.remove(&handle) {
        self.queue_script_task(event_loop, spec, source)?;
        continue;
      }
      if let Some(idx) = self.in_order_asap_by_handle.remove(&handle) {
        let entry = self.in_order_asap_scripts.get_mut(idx).ok_or_else(|| {
          Error::Other(format!(
            "internal error: in_order_asap_by_handle index out of bounds (idx={idx})"
          ))
        })?;
        entry.source = Some(source);
        continue;
      }
      if let Some(idx) = self.module_in_order_by_handle.remove(&handle) {
        let entry = self.module_in_order_scripts.get_mut(idx).ok_or_else(|| {
          Error::Other(format!(
            "internal error: module_in_order_by_handle index out of bounds (idx={idx})"
          ))
        })?;
        entry.source = Some(source);
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

    self.queue_ready_in_order_asap(event_loop)?;
    self.queue_ready_deferred(event_loop)?;
    self.queue_ready_module_in_order(event_loop)?;
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
      self.options.check_script_source(
        &source,
        &format!("source=external defer_idx={}", self.next_defer_to_queue),
      )?;
      let spec = entry
        .spec
        .take()
        .ok_or_else(|| Error::Other("internal error: deferred script missing spec".to_string()))?;
      self.next_defer_to_queue += 1;
      self.queue_script_task(event_loop, spec, source)?;
    }
    Ok(())
  }

  fn queue_ready_in_order_asap(&mut self, event_loop: &mut EventLoop<Host>) -> Result<()> {
    while self.next_in_order_asap_to_queue < self.in_order_asap_scripts.len() {
      let entry = &mut self.in_order_asap_scripts[self.next_in_order_asap_to_queue];
      let Some(source) = entry.source.take() else {
        break;
      };
      self.options.check_script_source(
        &source,
        &format!(
          "source=external in_order_asap_idx={}",
          self.next_in_order_asap_to_queue
        ),
      )?;
      let spec = entry
        .spec
        .take()
        .ok_or_else(|| Error::Other("internal error: in-order-asap script missing spec".to_string()))?;
      self.next_in_order_asap_to_queue += 1;
      self.queue_script_task(event_loop, spec, source)?;
    }
    Ok(())
  }

  fn queue_ready_module_in_order(&mut self, event_loop: &mut EventLoop<Host>) -> Result<()> {
    while self.next_module_in_order_to_queue < self.module_in_order_scripts.len() {
      let entry = &mut self.module_in_order_scripts[self.next_module_in_order_to_queue];
      let Some(source) = entry.source.take() else {
        break;
      };
      self.options.check_script_source(
        &source,
        &format!(
          "source=external module_in_order_idx={}",
          self.next_module_in_order_to_queue
        ),
      )?;
      let spec = entry.spec.take().ok_or_else(|| {
        Error::Other("internal error: in-order module script missing spec".to_string())
      })?;
      self.next_module_in_order_to_queue += 1;
      self.queue_script_task(event_loop, spec, source)?;
    }
    Ok(())
  }

  fn queue_script_task(
    &mut self,
    event_loop: &mut EventLoop<Host>,
    spec: ScriptElementSpec,
    source: String,
  ) -> Result<()> {
    let js_execution_depth = Rc::clone(&self.js_execution_depth);
    let task_source = match spec.script_type {
      ScriptType::Classic => TaskSource::Script,
      ScriptType::Module => TaskSource::Networking,
      ScriptType::ImportMap | ScriptType::Unknown => TaskSource::Script,
    };
    event_loop.queue_task(task_source, move |host, event_loop| {
      let _guard = enter_js_execution(&js_execution_depth);
      match spec.script_type {
        ScriptType::Classic => host.execute_classic_script(&source, &spec, event_loop)?,
        ScriptType::Module => host.execute_module_script(&source, &spec, event_loop)?,
        ScriptType::ImportMap | ScriptType::Unknown => {}
      }

      // HTML: external scripts (those with a `src` attribute) fire a `load` event once they have
      // finished executing. This is queued as an element task on the DOM manipulation task source.
      //
      // We model only the external-script case here: inline scripts do not queue load/error element
      // tasks in our current host pipeline.
      if spec.src_attr_present {
        let spec_for_event = spec.clone();
        event_loop.queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
          host.dispatch_script_event(ScriptElementEvent::Load, &spec_for_event)
        })?;
      }

      Ok(())
    })?;
    Ok(())
  }
}

/// A deterministic, event-driven scheduler for HTML `<script>` elements.
///
/// This models a subset of the HTML Standard script processing model:
/// - Inline classic scripts execute immediately and block parsing.
/// - External classic scripts:
///   - parser-inserted, no `async`/`defer`: parsing-blocking (execute immediately on fetch
///     completion).
///   - parser-inserted + `defer` (and not async-like): execute after parsing completes, in document
///     order.
///   - non-parser-inserted, not async-like: execute in insertion order as soon as possible (HTML
///     "in-order-asap" list).
///   - async-like: execute ASAP on fetch completion, not ordered.
/// - Module scripts (`type="module"`):
///   - never block parsing (even when parser-inserted),
///   - never execute synchronously in the parser/DOM mutation call stack; they always execute from a
///     queued task once the module graph is ready,
///   - async-like when `async` is present or the per-element "force async" flag is set: execute ASAP
///     once ready (may run before parsing completes),
///   - otherwise:
///     - parser-inserted module scripts execute after parsing completes (deferred-by-default; the
///       `defer` attribute has no effect),
///     - non-parser-inserted module scripts execute in insertion order as soon as possible (HTML
///       "in-order-asap" list).
/// - Import maps (`type="importmap"`):
///   - only processed when [`JsExecutionOptions::supports_module_scripts`] is true,
///   - must be inline-only (`src` queues an `error` element task).
/// - A microtask checkpoint after each script execution (performed by the orchestrator).
///
/// Out of scope (intentionally not modeled here):
/// - CSP, stylesheet-blocking scripts, and `document.write`.
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScriptTraceMetadata<'a> {
  pub url: Option<&'a str>,
  pub async_attr: bool,
  pub defer_attr: bool,
  pub parser_inserted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredScript<NodeId> {
  pub id: ScriptId,
  pub actions: Vec<ScriptSchedulerAction<NodeId>>,
}

/// A DOM event to fire at a `<script>` element as part of the HTML script processing model.
///
/// HTML queues these as "element tasks" on the DOM manipulation task source (e.g. `load`/`error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptElementEvent {
  Load,
  Error,
}

impl ScriptElementEvent {
  pub fn as_type_str(self) -> &'static str {
    match self {
      ScriptElementEvent::Load => "load",
      ScriptElementEvent::Error => "error",
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptSchedulerAction<NodeId> {
  /// Begin fetching an external script.
  StartFetch {
    script_id: ScriptId,
    node_id: NodeId,
    url: String,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
  },
  /// Begin fetching/creating a module script graph for a module script (`type="module"`).
  ///
  /// The embedding is responsible for resolving/fetching the entry module + its dependencies. Once
  /// the module graph is ready, it must call [`ScriptScheduler::module_graph_ready`].
  ///
  /// This is emitted for both inline and external module scripts.
  StartModuleGraphFetch {
    script_id: ScriptId,
    node_id: NodeId,
    /// Entry URL for external module scripts; `None` for inline module scripts.
    url: Option<String>,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
  },
  /// Block the HTML parser until the referenced script has executed.
  ///
  /// This is emitted for parsing-blocking external scripts (no `async`/`defer`).
  BlockParserUntilExecuted {
    script_id: ScriptId,
    node_id: NodeId,
  },
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
  /// Queue an element task to fire a `load`/`error` event at the `<script>` element.
  ///
  /// This is used by the HTML `prepare a script` algorithm when a script finishes loading or when
  /// its `src` attribute is present but empty/invalid/unresolvable.
  QueueScriptEventTask {
    script_id: ScriptId,
    node_id: NodeId,
    event: ScriptElementEvent,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalMode {
  Blocking,
  Defer,
  Async,
  InOrderAsap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalScriptEntry<NodeId> {
  #[allow(dead_code)]
  node_id: NodeId,
  #[allow(dead_code)]
  base_url_at_discovery: Option<String>,
  #[allow(dead_code)]
  url: String,
  async_attr: bool,
  defer_attr: bool,
  parser_inserted: bool,
  mode: ExternalMode,
  fetch_completed: bool,
  source_text: Option<String>,
  queued_for_execution: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleMode {
  /// Execute as soon as possible once the module graph is ready (not ordered).
  Async,
  /// Execute after parsing completes, in document order with classic `defer` scripts.
  AfterParsing,
  /// Execute in insertion order as soon as possible once ready (non-parser-inserted, `async` absent).
  InOrder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModuleScriptEntry<NodeId> {
  #[allow(dead_code)]
  node_id: NodeId,
  #[allow(dead_code)]
  base_url_at_discovery: Option<String>,
  /// URL for external module scripts; `None` for inline module scripts.
  #[allow(dead_code)]
  url: Option<String>,
  async_attr: bool,
  defer_attr: bool,
  parser_inserted: bool,
  mode: ModuleMode,
  graph_ready: bool,
  source_text: Option<String>,
  queued_for_execution: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScriptEntry<NodeId> {
  ClassicExternal(ExternalScriptEntry<NodeId>),
  Module(ModuleScriptEntry<NodeId>),
}

pub struct ScriptScheduler<NodeId> {
  options: JsExecutionOptions,
  next_script_id: u64,
  scripts: HashMap<ScriptId, ScriptEntry<NodeId>>,
  /// Scripts that execute after parsing completes (classic `defer` + parser-inserted module scripts
  /// without `async`), in document order.
  defer_queue: Vec<ScriptId>,
  next_defer_to_queue: usize,
  /// Scripts that execute in insertion order as soon as possible once ready (HTML's
  /// "list of scripts that will execute in order as soon as possible").
  ///
  /// This can include both classic external scripts and module scripts.
  in_order_queue: Vec<ScriptId>,
  next_in_order_to_queue: usize,
  parsing_completed: bool,
}

impl<NodeId> Default for ScriptScheduler<NodeId> {
  fn default() -> Self {
    Self {
      options: JsExecutionOptions::default(),
      next_script_id: 1,
      scripts: HashMap::new(),
      defer_queue: Vec::new(),
      next_defer_to_queue: 0,
      in_order_queue: Vec::new(),
      next_in_order_to_queue: 0,
      parsing_completed: false,
    }
  }
}

impl<NodeId: Clone> ScriptScheduler<NodeId> {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_options(options: JsExecutionOptions) -> Self {
    Self { options, ..Self::default() }
  }

  pub fn set_options(&mut self, options: JsExecutionOptions) {
    self.options = options;
  }

  pub fn options(&self) -> JsExecutionOptions {
    self.options
  }

  pub(crate) fn trace_metadata(&self, script_id: ScriptId) -> Option<ScriptTraceMetadata<'_>> {
    let entry = self.scripts.get(&script_id)?;
    match entry {
      ScriptEntry::ClassicExternal(entry) => Some(ScriptTraceMetadata {
        url: Some(entry.url.as_str()),
        async_attr: entry.async_attr,
        defer_attr: entry.defer_attr,
        parser_inserted: entry.parser_inserted,
      }),
      ScriptEntry::Module(entry) => Some(ScriptTraceMetadata {
        url: entry.url.as_deref(),
        async_attr: entry.async_attr,
        defer_attr: entry.defer_attr,
        parser_inserted: entry.parser_inserted,
      }),
    }
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
    // Force the parser-inserted flag on this code path; callers building specs at parse time should
    // already set this, but doing it here keeps the API robust.
    let mut element = element;
    element.parser_inserted = true;
    element.force_async = false;
    self.discovered_script(element, node_id, base_url_at_discovery)
  }

  /// Notify the scheduler that a `<script>` element has been discovered.
  ///
  /// This is a more general form of [`discovered_parser_script`](Self::discovered_parser_script)
  /// that respects [`ScriptElementSpec::parser_inserted`]:
  ///
  /// - **Parser-inserted** external scripts may block parsing or be deferred.
  /// - **Non-parser-inserted** external scripts are async-like when `force_async` is true (the HTML
  ///   default for dynamically created scripts); otherwise they execute in insertion order as soon
  ///   as possible.
  pub fn discovered_script(
    &mut self,
    element: ScriptElementSpec,
    node_id: NodeId,
    base_url_at_discovery: Option<String>,
  ) -> Result<DiscoveredScript<NodeId>> {
    let id = self.alloc_script_id();
    let mut actions: Vec<ScriptSchedulerAction<NodeId>> = Vec::new();

    if element.is_suppressed_by_nomodule(&self.options) {
      return Ok(DiscoveredScript { id, actions });
    }

    // HTML: If there is no `src` attribute and the source text is empty, "prepare a script"
    // returns early.
    if !element.src_attr_present && element.inline_text.is_empty() {
      return Ok(DiscoveredScript { id, actions });
    }
 
    match element.script_type {
      ScriptType::Classic => {
        if element.src_attr_present {
          let Some(url) = element.src.filter(|s| !s.is_empty()) else {
            // HTML: if the `src` attribute is present but the value is the empty string or cannot be
            // resolved to a URL, queue an element task to fire `error`, then return. Crucially, the
            // presence of `src` suppresses inline script execution even in this failure case.
            actions.push(ScriptSchedulerAction::QueueScriptEventTask {
              script_id: id,
              node_id,
              event: ScriptElementEvent::Error,
            });
            return Ok(DiscoveredScript { id, actions });
          };
          let async_like = element.async_attr || element.force_async;
          let mode = if async_like {
            ExternalMode::Async
          } else if !element.parser_inserted {
            ExternalMode::InOrderAsap
          } else if element.defer_attr {
            ExternalMode::Defer
          } else {
            ExternalMode::Blocking
          };

          if mode == ExternalMode::Defer {
            self.defer_queue.push(id);
          }
          if mode == ExternalMode::InOrderAsap {
            self.in_order_queue.push(id);
          }

          self.scripts.insert(
            id,
            ScriptEntry::ClassicExternal(ExternalScriptEntry {
              node_id: node_id.clone(),
              base_url_at_discovery,
              url: url.clone(),
              async_attr: element.async_attr,
              defer_attr: element.defer_attr,
              parser_inserted: element.parser_inserted,
              mode,
              fetch_completed: false,
              source_text: None,
              queued_for_execution: false,
            }),
          );

          let (destination, credentials_mode) = if let Some(cors_mode) = element.crossorigin {
            (FetchDestination::ScriptCors, cors_mode.credentials_mode())
          } else {
            (FetchDestination::Script, FetchCredentialsMode::Include)
          };
          actions.push(ScriptSchedulerAction::StartFetch {
            script_id: id,
            node_id: node_id.clone(),
            url,
            destination,
            credentials_mode,
          });

          // Only parser-inserted blocking scripts are allowed to block parsing.
          if mode == ExternalMode::Blocking {
            actions.push(ScriptSchedulerAction::BlockParserUntilExecuted { script_id: id, node_id });
          }
        } else {
          // Inline classic scripts execute synchronously during preparation (HTML "prepare a
          // script"), both for parser-inserted scripts and for dynamically inserted scripts.
          //
          // Observable behavior in browsers:
          // - `document.body.appendChild(scriptWithText)` runs the inline script before
          //   `appendChild` returns.
          actions.push(ScriptSchedulerAction::ExecuteNow {
            script_id: id,
            node_id,
            source_text: element.inline_text,
          });
        }
      }
      ScriptType::Module => {
        // Even when module execution is disabled, HTML still requires that an empty/invalid `src`
        // attribute queues an `error` event task (and the element must not fall back to inline
        // execution).
        //
        // Keep this check outside the `supports_module_scripts` gate so hosts that ignore module
        // scripts still get the correct error behavior.
        if element.src_attr_present && element.src.as_deref().filter(|s| !s.is_empty()).is_none() {
          actions.push(ScriptSchedulerAction::QueueScriptEventTask {
            script_id: id,
            node_id,
            event: ScriptElementEvent::Error,
          });
          return Ok(DiscoveredScript { id, actions });
        }

        if !self.options.supports_module_scripts {
          // Treat module scripts as unsupported when the embedding does not enable module execution.
          // This mirrors browser behavior where unsupported `<script type="module">` is ignored.
          return Ok(DiscoveredScript { id, actions });
        }

        // Module scripts never execute synchronously in the parser/DOM mutation call stack; they
        // are always executed from a queued task once the module graph is ready.
        //
        // For ordering, HTML treats module scripts as async-like when either:
        // - the `async` attribute is present, or
        // - the element's internal "force async" flag is set (the default for dynamically created
        //   scripts).
        let mode = if element.is_effectively_async() {
          ModuleMode::Async
        } else if element.parser_inserted {
          // Parser-inserted module scripts are deferred-by-default.
          ModuleMode::AfterParsing
        } else {
          // Dynamic module scripts without `async` execute in insertion order as soon as possible once ready.
          ModuleMode::InOrder
        };

        let url_for_entry: Option<String> = if element.src_attr_present {
          match element.src.filter(|s| !s.is_empty()) {
            Some(url) => Some(url),
            None => {
              // HTML: module scripts with `src` present but empty/invalid also queue `error` and do not
              // fall back to inline execution.
              actions.push(ScriptSchedulerAction::QueueScriptEventTask {
                script_id: id,
                node_id,
                event: ScriptElementEvent::Error,
              });
              return Ok(DiscoveredScript { id, actions });
            }
          }
        } else {
          None
        };

        if mode == ModuleMode::AfterParsing {
          self.defer_queue.push(id);
        } else if mode == ModuleMode::InOrder {
          self.in_order_queue.push(id);
        }

        // Module scripts are fetched in CORS mode and default to `same-origin` credentials. The
        // `crossorigin` CORS settings attribute only affects the credentials mode.
        let destination = FetchDestination::ScriptCors;
        let credentials_mode = element
          .crossorigin
          .map(|cors_mode| cors_mode.credentials_mode())
          .unwrap_or(FetchCredentialsMode::SameOrigin);

        let action_url = url_for_entry.clone();
        self.scripts.insert(
          id,
          ScriptEntry::Module(ModuleScriptEntry {
            node_id: node_id.clone(),
            base_url_at_discovery,
            url: url_for_entry,
            async_attr: element.async_attr,
            defer_attr: element.defer_attr,
            parser_inserted: element.parser_inserted,
            mode,
            graph_ready: false,
            source_text: None,
            queued_for_execution: false,
          }),
        );

        actions.push(ScriptSchedulerAction::StartModuleGraphFetch {
          script_id: id,
          node_id,
          url: action_url,
          destination,
          credentials_mode,
        });
      }
      ScriptType::ImportMap => {
        // Import maps are only meaningful when module scripts are supported. When module scripts are
        // disabled, treat `type="importmap"` the same way browsers without module support do: ignore
        // it as an unknown script type (no execution and no load/error events).
        if !self.options.supports_module_scripts {
          return Ok(DiscoveredScript { id, actions });
        }

        // HTML: import maps must be inline; `src` is invalid and must queue `error`.
        if element.src_attr_present {
          actions.push(ScriptSchedulerAction::QueueScriptEventTask {
            script_id: id,
            node_id,
            event: ScriptElementEvent::Error,
          });
          return Ok(DiscoveredScript { id, actions });
        }

        // Inline import maps are processed synchronously when parser-inserted; dynamically inserted
        // import maps execute as tasks (mirroring classic-script async-by-default behavior).
        if element.parser_inserted {
          actions.push(ScriptSchedulerAction::ExecuteNow {
            script_id: id,
            node_id,
            source_text: element.inline_text,
          });
        } else {
          actions.push(ScriptSchedulerAction::QueueTask {
            script_id: id,
            node_id,
            source_text: element.inline_text,
          });
        }
      }
      ScriptType::Unknown => {
        // Unknown script types do not execute.
        return Ok(DiscoveredScript { id, actions });
      }
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
    let ScriptEntry::ClassicExternal(entry) = entry else {
      return Err(Error::Other(format!(
        "fetch_completed called for non-classic script_id={}",
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
        let node_id = entry.node_id.clone();
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
        let node_id = entry.node_id.clone();
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
      ExternalMode::InOrderAsap => self.queue_in_order_scripts_if_ready(),
    }
  }

  /// Notify the scheduler that the module graph for a module script is ready.
  ///
  /// This corresponds to HTML's notion of a "ready" module script (entry module + dependencies
  /// fetched/created). Callers should invoke this once the module graph is fully available.
  pub fn module_graph_ready(
    &mut self,
    script_id: ScriptId,
    source_text: String,
  ) -> Result<Vec<ScriptSchedulerAction<NodeId>>> {
    let Some(entry) = self.scripts.get_mut(&script_id) else {
      return Err(Error::Other(format!(
        "module_graph_ready called for unknown script_id={}",
        script_id.as_u64()
      )));
    };
    let ScriptEntry::Module(entry) = entry else {
      return Err(Error::Other(format!(
        "module_graph_ready called for non-module script_id={}",
        script_id.as_u64()
      )));
    };

    if entry.graph_ready {
      return Err(Error::Other(format!(
        "module_graph_ready called more than once for script_id={}",
        script_id.as_u64()
      )));
    }
    entry.graph_ready = true;
    entry.source_text = Some(source_text);

    match entry.mode {
      ModuleMode::Async => {
        if entry.queued_for_execution {
          return Ok(Vec::new());
        }
        entry.queued_for_execution = true;
        let node_id = entry.node_id.clone();
        let source_text = entry.source_text.take().ok_or_else(|| {
          Error::Other("internal error: missing source text after module_graph_ready".to_string())
        })?;
        Ok(vec![ScriptSchedulerAction::QueueTask {
          script_id,
          node_id,
          source_text,
        }])
      }
      ModuleMode::AfterParsing => self.queue_defer_scripts_if_ready(),
      ModuleMode::InOrder => self.queue_in_order_scripts_if_ready(),
    }
  }

  /// Notify the scheduler that building/fetching the module graph for a module script failed.
  ///
  /// Like [`ScriptScheduler::fetch_failed`] for classic scripts, failed module scripts are treated
  /// as "completed" for ordering purposes: they must not execute, but they must not block later
  /// ordered scripts (in-order-asap dynamic modules or after-parsing scripts).
  pub fn module_graph_failed(
    &mut self,
    script_id: ScriptId,
  ) -> Result<Vec<ScriptSchedulerAction<NodeId>>> {
    let mode = {
      let Some(entry) = self.scripts.get_mut(&script_id) else {
        return Err(Error::Other(format!(
          "module_graph_failed called for unknown script_id={}",
          script_id.as_u64()
        )));
      };
      let ScriptEntry::Module(entry) = entry else {
        return Err(Error::Other(format!(
          "module_graph_failed called for non-module script_id={}",
          script_id.as_u64()
        )));
      };
      if entry.graph_ready {
        return Err(Error::Other(format!(
          "module_graph_failed called after module graph readiness for script_id={}",
          script_id.as_u64()
        )));
      }
      entry.graph_ready = true;
      entry.queued_for_execution = true;
      entry.source_text = None;
      entry.mode
    };

    match mode {
      ModuleMode::AfterParsing => self.queue_defer_scripts_if_ready(),
      ModuleMode::InOrder => self.queue_in_order_scripts_if_ready(),
      ModuleMode::Async => Ok(Vec::new()),
    }
  }

  /// Notify the scheduler that an external script fetch failed (network error, CORS failure, SRI
  /// mismatch, etc).
  ///
  /// Failed scripts are treated as "completed" for ordering purposes:
  /// - the script must be treated as "done" (it must not execute),
  /// - parser-blocking scripts must unblock parsing (handled by the host),
  /// - deferred scripts are skipped so later deferred scripts can still run.
  ///
  /// The scheduler does not currently surface the failure reason; the host is expected to dispatch
  /// an `error` event at the script element.
  pub fn fetch_failed(&mut self, script_id: ScriptId) -> Result<Vec<ScriptSchedulerAction<NodeId>>> {
    let mode = {
      let Some(entry) = self.scripts.get_mut(&script_id) else {
        return Err(Error::Other(format!(
          "fetch_failed called for unknown script_id={}",
          script_id.as_u64()
        )));
      };
      let ScriptEntry::ClassicExternal(entry) = entry else {
        return Err(Error::Other(format!(
          "fetch_failed called for non-classic script_id={}",
          script_id.as_u64()
        )));
      };

      if entry.fetch_completed {
        return Err(Error::Other(format!(
          "fetch_failed called after fetch completion for script_id={}",
          script_id.as_u64()
        )));
      }

      entry.fetch_completed = true;
      entry.queued_for_execution = true;
      entry.source_text = None;
      entry.mode
    };

    match mode {
      ExternalMode::Defer => self.queue_defer_scripts_if_ready(),
      ExternalMode::InOrderAsap => self.queue_in_order_scripts_if_ready(),
      ExternalMode::Blocking | ExternalMode::Async => Ok(Vec::new()),
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

      match entry {
        ScriptEntry::ClassicExternal(entry) => {
          if entry.queued_for_execution {
            self.next_defer_to_queue += 1;
            continue;
          }

          if entry.source_text.is_none() {
            break;
          }

          entry.queued_for_execution = true;
          let node_id = entry.node_id.clone();
          let source_text = entry.source_text.take().ok_or_else(|| {
            Error::Other(
              "internal error: missing source text when queueing after-parsing scripts".to_string(),
            )
          })?;
          actions.push(ScriptSchedulerAction::QueueTask {
            script_id,
            node_id,
            source_text,
          });
          self.next_defer_to_queue += 1;
        }
        ScriptEntry::Module(entry) => {
          if entry.queued_for_execution {
            self.next_defer_to_queue += 1;
            continue;
          }

          if entry.source_text.is_none() {
            break;
          }

          entry.queued_for_execution = true;
          let node_id = entry.node_id.clone();
          let source_text = entry.source_text.take().ok_or_else(|| {
            Error::Other(
              "internal error: missing source text when queueing after-parsing scripts".to_string(),
            )
          })?;
          actions.push(ScriptSchedulerAction::QueueTask {
            script_id,
            node_id,
            source_text,
          });
          self.next_defer_to_queue += 1;
        }
      }
    }

    Ok(actions)
  }

  fn queue_in_order_scripts_if_ready(&mut self) -> Result<Vec<ScriptSchedulerAction<NodeId>>> {
    let mut actions: Vec<ScriptSchedulerAction<NodeId>> = Vec::new();
    while self.next_in_order_to_queue < self.in_order_queue.len() {
      let script_id = self.in_order_queue[self.next_in_order_to_queue];
      let Some(entry) = self.scripts.get_mut(&script_id) else {
        return Err(Error::Other(format!(
          "internal error: in_order_queue references missing script_id={}",
          script_id.as_u64()
        )));
      };
      match entry {
        ScriptEntry::ClassicExternal(entry) => {
          if entry.queued_for_execution {
            self.next_in_order_to_queue += 1;
            continue;
          }

          if entry.source_text.is_none() {
            break;
          }

          entry.queued_for_execution = true;
          let node_id = entry.node_id.clone();
          let source_text = entry.source_text.take().ok_or_else(|| {
            Error::Other(
              "internal error: missing source text when queueing in-order-asap scripts".to_string(),
            )
          })?;
          actions.push(ScriptSchedulerAction::QueueTask {
            script_id,
            node_id,
            source_text,
          });
          self.next_in_order_to_queue += 1;
        }
        ScriptEntry::Module(entry) => {
          debug_assert_eq!(entry.mode, ModuleMode::InOrder);

          if entry.queued_for_execution {
            self.next_in_order_to_queue += 1;
            continue;
          }

          if entry.source_text.is_none() {
            break;
          }

          entry.queued_for_execution = true;
          let node_id = entry.node_id.clone();
          let source_text = entry.source_text.take().ok_or_else(|| {
            Error::Other(
              "internal error: missing source text when queueing in-order-asap scripts".to_string(),
            )
          })?;
          actions.push(ScriptSchedulerAction::QueueTask {
            script_id,
            node_id,
            source_text,
          });
          self.next_in_order_to_queue += 1;
        }
      }
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
  use std::cell::RefCell;
  use std::collections::VecDeque;
  use std::rc::Rc;

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
    events: Vec<String>,
    queue_microtask_after_execute: bool,
  }

  impl TestHost {
    fn new(queue_microtask_after_execute: bool) -> Self {
      Self {
        loader: ManualLoader::default(),
        log: Vec::new(),
        events: Vec::new(),
        queue_microtask_after_execute,
      }
    }
  }

  impl ScriptLoader for TestHost {
    type Handle = usize;

    fn load_blocking(
      &mut self,
      url: &str,
      _destination: FetchDestination,
      _credentials_mode: FetchCredentialsMode,
    ) -> Result<String> {
      self
        .loader
        .blocking_sources
        .get(url)
        .cloned()
        .ok_or_else(|| crate::error::Error::Other(format!("no blocking source for url={url}")))
    }

    fn start_load(
      &mut self,
      url: &str,
      _destination: FetchDestination,
      _credentials_mode: FetchCredentialsMode,
    ) -> Result<Self::Handle> {
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

    fn execute_module_script(
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

  impl ScriptEventDispatcher for TestHost {
    fn dispatch_script_event(
      &mut self,
      event: ScriptElementEvent,
      _spec: &ScriptElementSpec,
    ) -> Result<()> {
      self.events.push(event.as_type_str().to_string());
      Ok(())
    }
  }

  fn inline_script(text: &str) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: None,
      src_attr_present: false,
      inline_text: text.to_string(),
      async_attr: false,
      defer_attr: false,
      nomodule_attr: false,
      force_async: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: true,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  fn external_script(url: &str, async_attr: bool, defer_attr: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: Some(url.to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr,
      defer_attr,
      nomodule_attr: false,
      force_async: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: true,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  fn external_script_dynamic(
    url: &str,
    async_attr: bool,
    defer_attr: bool,
    force_async: bool,
  ) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: Some(url.to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr,
      defer_attr,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: false,
      force_async,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  #[derive(Default)]
  struct FetchSemanticsHost {
    blocking: Vec<(String, FetchDestination, FetchCredentialsMode)>,
    started: Vec<(String, FetchDestination, FetchCredentialsMode)>,
    next_handle: usize,
  }

  impl ScriptLoader for FetchSemanticsHost {
    type Handle = usize;

    fn load_blocking(
      &mut self,
      url: &str,
      destination: FetchDestination,
      credentials_mode: FetchCredentialsMode,
    ) -> Result<String> {
      self
        .blocking
        .push((url.to_string(), destination, credentials_mode));
      Ok(String::new())
    }

    fn start_load(
      &mut self,
      url: &str,
      destination: FetchDestination,
      credentials_mode: FetchCredentialsMode,
    ) -> Result<Self::Handle> {
      self
        .started
        .push((url.to_string(), destination, credentials_mode));
      let handle = self.next_handle;
      self.next_handle = self.next_handle.wrapping_add(1);
      Ok(handle)
    }

    fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>> {
      Ok(None)
    }
  }

  impl ScriptExecutor for FetchSemanticsHost {
    fn execute_classic_script(
      &mut self,
      _script_text: &str,
      _spec: &ScriptElementSpec,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      _script_text: &str,
      _spec: &ScriptElementSpec,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      Ok(())
    }
  }

  impl ScriptEventDispatcher for FetchSemanticsHost {
    fn dispatch_script_event(
      &mut self,
      _event: ScriptElementEvent,
      _spec: &ScriptElementSpec,
    ) -> Result<()> {
      Ok(())
    }
  }

  #[test]
  fn classic_scripts_fetch_no_cors_and_include_credentials_by_default() -> Result<()> {
    let mut host = FetchSemanticsHost::default();
    let mut event_loop = EventLoop::<FetchSemanticsHost>::new();
    let mut scheduler = ClassicScriptScheduler::<FetchSemanticsHost>::new();

    // Parser-inserted, no async/defer -> blocking external script uses load_blocking.
    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script("https://example.com/a.js", false, false),
    )?;

    assert_eq!(
      host.blocking,
      vec![(
        "https://example.com/a.js".to_string(),
        FetchDestination::Script,
        FetchCredentialsMode::Include,
      )]
    );
    Ok(())
  }

  #[test]
  fn classic_crossorigin_scripts_fetch_cors_and_map_credentials_mode() -> Result<()> {
    let mut host = FetchSemanticsHost::default();
    let mut event_loop = EventLoop::<FetchSemanticsHost>::new();
    let mut scheduler = ClassicScriptScheduler::<FetchSemanticsHost>::new();

    let mut spec = external_script("https://example.com/a.js", false, false);
    spec.crossorigin = Some(crate::resource::CorsMode::Anonymous);
    scheduler.handle_script(&mut host, &mut event_loop, spec)?;

    assert_eq!(
      host.blocking,
      vec![(
        "https://example.com/a.js".to_string(),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::SameOrigin,
      )]
    );
    Ok(())
  }

  #[test]
  fn module_scripts_fetch_cors_and_same_origin_credentials_by_default() -> Result<()> {
    let mut host = FetchSemanticsHost::default();
    let mut event_loop = EventLoop::<FetchSemanticsHost>::new();
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut scheduler = ClassicScriptScheduler::<FetchSemanticsHost>::with_options(options);

    let mut spec = external_script("https://example.com/a.js", false, false);
    spec.script_type = ScriptType::Module;
    scheduler.handle_script(&mut host, &mut event_loop, spec)?;

    assert_eq!(
      host.started,
      vec![(
        "https://example.com/a.js".to_string(),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::SameOrigin,
      )]
    );
    Ok(())
  }

  #[test]
  fn nomodule_scripts_execute_when_module_scripts_not_supported() -> Result<()> {
    // When the runtime does not support module scripts, the `nomodule` attribute must have no
    // effect: browsers without module support still execute these scripts.
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    let mut spec = inline_script("RUN");
    spec.nomodule_attr = true;

    scheduler.handle_script(&mut host, &mut event_loop, spec)?;
    assert_eq!(host.log, vec!["RUN".to_string()]);
    Ok(())
  }

  #[test]
  fn non_parser_inserted_external_scripts_execute_without_waiting_for_parsing_complete() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    // Dynamically inserted external scripts whose force-async flag is set behave like `async`
    // scripts, regardless of the `defer` attribute. They should execute as soon as their load
    // completes, even if parsing has not finished.
    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      ScriptElementSpec {
        base_url: None,
        src: Some("dyn".to_string()),
        src_attr_present: true,
        inline_text: String::new(),
        async_attr: false,
        defer_attr: true,
        nomodule_attr: false,
        force_async: true,
        crossorigin: None,
        integrity_attr_present: false,
        integrity: None,
        referrer_policy: None,
        parser_inserted: false,
        node_id: None,
        script_type: ScriptType::Classic,
      },
    )?;

    host.loader.complete_url("dyn", "dyn");
    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["dyn".to_string()]);
    Ok(())
  }

  #[test]
  fn dynamic_in_order_asap_external_scripts_execute_in_insertion_order() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script_dynamic("A", /* async_attr */ false, /* defer_attr */ false, /* force_async */ false),
    )?;
    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script_dynamic("B", /* async_attr */ false, /* defer_attr */ false, /* force_async */ false),
    )?;

    // Complete out-of-order: B finishes before A.
    host.loader.complete_url("B", "B");
    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.log, Vec::<String>::new());

    host.loader.complete_url("A", "A");
    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["A".to_string(), "B".to_string()]);
    Ok(())
  }

  #[test]
  fn dynamic_async_scripts_can_execute_before_in_order_asap_scripts() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script_dynamic("A", /* async_attr */ false, /* defer_attr */ false, /* force_async */ false),
    )?;
    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script_dynamic("B", /* async_attr */ true, /* defer_attr */ false, /* force_async */ false),
    )?;

    // B is async-like and can run before A if it completes first.
    host.loader.complete_url("B", "B");
    host.loader.complete_url("A", "A");
    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["B".to_string(), "A".to_string()]);
    Ok(())
  }

  #[test]
  fn dynamic_force_async_true_behaves_like_async() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script_dynamic("A", /* async_attr */ false, /* defer_attr */ false, /* force_async */ true),
    )?;
    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script_dynamic("B", /* async_attr */ false, /* defer_attr */ false, /* force_async */ true),
    )?;

    // Async scripts execute in completion order.
    host.loader.complete_url("B", "B");
    host.loader.complete_url("A", "A");
    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["B".to_string(), "A".to_string()]);
    Ok(())
  }

  #[test]
  fn async_attribute_overrides_defer_for_external_scripts() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      ScriptElementSpec {
        base_url: None,
        src: Some("a1".to_string()),
        src_attr_present: true,
        inline_text: String::new(),
        async_attr: true,
        defer_attr: true,
        nomodule_attr: false,
        force_async: false,
        crossorigin: None,
        integrity_attr_present: false,
        integrity: None,
        referrer_policy: None,
        parser_inserted: true,
        node_id: None,
        script_type: ScriptType::Classic,
      },
    )?;

    host.loader.complete_url("a1", "a1");
    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["a1".to_string()]);
    Ok(())
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
  fn src_attribute_present_suppresses_inline_fallback_when_src_url_is_missing() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    // HTML: if the `src` attribute is present but empty/invalid, the script element does not fall
    // back to executing its inline child text content.
    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      ScriptElementSpec {
        base_url: None,
        // Invalid/empty `src` -> no resolved URL.
        src: None,
        src_attr_present: true,
        inline_text: "INLINE".to_string(),
        async_attr: false,
        defer_attr: false,
        nomodule_attr: false,
        force_async: false,
        crossorigin: None,
        integrity_attr_present: false,
        integrity: None,
        referrer_policy: None,
        parser_inserted: true,
        node_id: None,
        script_type: ScriptType::Classic,
      },
    )?;

    assert_eq!(
      host.log,
      Vec::<String>::new(),
      "expected no inline execution when src attribute is present"
    );
    assert!(
      host.loader.handles_by_url.is_empty(),
      "expected no fetch to be started for invalid src"
    );
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(
      host.events,
      vec!["error".to_string()],
      "expected an error event task for invalid src"
    );
    Ok(())
  }

  #[test]
  fn nomodule_inline_script_executes_when_module_scripts_unsupported() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    let mut spec = inline_script("RUN");
    spec.nomodule_attr = true;
    scheduler.handle_script(&mut host, &mut event_loop, spec)?;

    assert_eq!(host.log, vec!["RUN".to_string()]);
    Ok(())
  }

  #[test]
  fn nomodule_external_script_executes_when_module_scripts_unsupported() -> Result<()> {
    let mut host = TestHost::new(false);
    host
      .loader
      .blocking_sources
      .insert("ext.js".to_string(), "EXT".to_string());
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    let mut spec = external_script("ext.js", false, false);
    spec.nomodule_attr = true;
    scheduler.handle_script(&mut host, &mut event_loop, spec)?;

    assert_eq!(host.log, vec!["EXT".to_string()]);
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
  fn external_script_queues_load_event_task_after_execution() -> Result<()> {
    let mut host = TestHost::new(true);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    scheduler.handle_script(
      &mut host,
      &mut event_loop,
      external_script("a1", /* async_attr */ true, /* defer_attr */ false),
    )?;

    host.loader.complete_url("a1", "a1");
    scheduler.poll(&mut host, &mut event_loop)?;

    // First task turn: execute the script task (and its microtask checkpoint).
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(
      host.log,
      vec!["a1".to_string(), "microtask-after-a1".to_string()]
    );
    assert!(
      host.events.is_empty(),
      "load event must be queued as a separate element task"
    );

    // Second task turn: run the queued `load` element task.
    assert!(event_loop.run_next_task(&mut host)?);
    assert_eq!(host.events, vec!["load".to_string()]);
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
  fn microtasks_run_before_parser_blocking_inline_script_even_inside_parse_task() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let scheduler = Rc::new(RefCell::new(ClassicScriptScheduler::<TestHost>::new()));

    event_loop.queue_microtask(|host, _event_loop| {
      host.log.push("microtask".to_string());
      Ok(())
    })?;

    let scheduler_for_task = Rc::clone(&scheduler);
    event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
      scheduler_for_task
        .borrow_mut()
        .handle_script(host, event_loop, inline_script("RUN"))?;
      Ok(())
    })?;

    // Run the parse task first (without pre-draining microtasks) to ensure the pre-script checkpoint
    // at `</script>` boundaries is the mechanism that flushes the microtask.
    assert!(event_loop.run_next_task(&mut host)?);
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.log, vec!["microtask".to_string(), "RUN".to_string()]);
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

  #[test]
  fn nomodule_inline_script_executes_when_module_scripts_not_supported() -> Result<()> {
    let mut host = TestHost::new(false);
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    let mut spec = inline_script("RUN");
    spec.nomodule_attr = true;
    scheduler.handle_script(&mut host, &mut event_loop, spec)?;

    assert_eq!(host.log, vec!["RUN".to_string()]);
    Ok(())
  }

  #[test]
  fn nomodule_external_script_executes_when_module_scripts_not_supported() -> Result<()> {
    let mut host = TestHost::new(false);
    host
      .loader
      .blocking_sources
      .insert("https://example.com/a.js".to_string(), "A".to_string());
    let mut event_loop = EventLoop::<TestHost>::new();
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();

    let mut spec = external_script("https://example.com/a.js", false, false);
    spec.nomodule_attr = true;
    scheduler.handle_script(&mut host, &mut event_loop, spec)?;

    assert_eq!(host.log, vec!["A".to_string()]);
    Ok(())
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
      src_attr_present: false,
      inline_text: text.to_string(),
      async_attr: false,
      defer_attr: false,
      nomodule_attr: false,
      force_async: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: true,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  fn classic_external(src: &str, async_attr: bool, defer_attr: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: Some(src.to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr,
      defer_attr,
      nomodule_attr: false,
      force_async: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: true,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  fn classic_external_dynamic(
    src: &str,
    async_attr: bool,
    defer_attr: bool,
    force_async: bool,
  ) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: Some(src.to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr,
      defer_attr,
      nomodule_attr: false,
      force_async,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: false,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  fn classic_inline_dynamic(text: &str) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: None,
      src_attr_present: false,
      inline_text: text.to_string(),
      async_attr: false,
      defer_attr: false,
      nomodule_attr: false,
      force_async: true,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: false,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  fn module_inline(text: &str) -> ScriptElementSpec {
    let mut spec = classic_inline(text);
    spec.script_type = ScriptType::Module;
    spec
  }

  fn module_external(src: &str, async_attr: bool) -> ScriptElementSpec {
    let mut spec = classic_external(src, async_attr, /* defer_attr */ false);
    spec.script_type = ScriptType::Module;
    spec
  }

  fn module_external_dynamic(src: &str, async_attr: bool, force_async: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: Some(src.to_string()),
      src_attr_present: true,
      inline_text: String::new(),
      async_attr,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: false,
      force_async,
      node_id: None,
      script_type: ScriptType::Module,
    }
  }

  fn module_inline_dynamic(text: &str, async_attr: bool, force_async: bool) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: None,
      src_attr_present: false,
      inline_text: text.to_string(),
      async_attr,
      force_async,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: false,
      node_id: None,
      script_type: ScriptType::Module,
    }
  }

  fn importmap_inline(text: &str) -> ScriptElementSpec {
    let mut spec = classic_inline(text);
    spec.script_type = ScriptType::ImportMap;
    spec
  }

  fn importmap_inline_dynamic(text: &str) -> ScriptElementSpec {
    let mut spec = classic_inline_dynamic(text);
    spec.script_type = ScriptType::ImportMap;
    spec
  }

  fn with_nomodule(mut spec: ScriptElementSpec) -> ScriptElementSpec {
    spec.nomodule_attr = true;
    spec
  }

  struct Harness {
    scheduler: ScriptScheduler<u32>,
    event_loop: EventLoop<Host>,
    host: Host,
    started_fetches: Vec<(ScriptId, u32, String, FetchDestination, FetchCredentialsMode)>,
    started_module_graph_fetches:
      Vec<(ScriptId, u32, Option<String>, FetchDestination, FetchCredentialsMode)>,
    blocked_parser_on: Option<ScriptId>,
    script_type_by_id: HashMap<ScriptId, ScriptType>,
  }

  impl Harness {
    fn new() -> Self {
      Self::new_with_options(JsExecutionOptions::default())
    }

    fn new_with_options(options: JsExecutionOptions) -> Self {
      Self {
        scheduler: ScriptScheduler::with_options(options),
        event_loop: EventLoop::new(),
        host: Host::default(),
        started_fetches: Vec::new(),
        started_module_graph_fetches: Vec::new(),
        blocked_parser_on: None,
        script_type_by_id: HashMap::new(),
      }
    }

    fn apply_actions(&mut self, actions: Vec<ScriptSchedulerAction<u32>>) -> Result<()> {
      for action in actions {
        match action {
          ScriptSchedulerAction::StartFetch {
            script_id,
            node_id,
            url,
            destination,
            credentials_mode,
          } => {
            self
              .started_fetches
              .push((script_id, node_id, url, destination, credentials_mode));
          }
          ScriptSchedulerAction::StartModuleGraphFetch {
            script_id,
            node_id,
            url,
            destination,
            credentials_mode,
          } => {
            self
              .started_module_graph_fetches
              .push((script_id, node_id, url, destination, credentials_mode));
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
            script_id,
            node_id: _,
            source_text,
          } => {
            let task_source = self
              .script_type_by_id
              .get(&script_id)
              .copied()
              .map(|ty| match ty {
                ScriptType::Module => TaskSource::Networking,
                _ => TaskSource::Script,
              })
              .unwrap_or(TaskSource::Script);
            self
              .event_loop
              .queue_task(task_source, move |host, event_loop| {
                execute_fake_script(host, event_loop, &source_text)
              })?;
          }
          ScriptSchedulerAction::QueueScriptEventTask {
            script_id: _,
            node_id: _,
            event,
          } => {
            let type_str = event.as_type_str();
            self
              .event_loop
              .queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
                host.log.push(format!("event:{type_str}"));
                Ok(())
              })?;
          }
        }
      }
      Ok(())
    }

    fn discover(&mut self, element: ScriptElementSpec) -> Result<ScriptId> {
      let script_type = element.script_type;
      let discovered = self.scheduler.discovered_parser_script(
        element, /* node_id */ 1, /* base_url_at_discovery */ None,
      )?;
      let id = discovered.id;
      self.script_type_by_id.insert(id, script_type);
      self.apply_actions(discovered.actions)?;
      Ok(id)
    }

    fn discover_dynamic(&mut self, element: ScriptElementSpec) -> Result<ScriptId> {
      let script_type = element.script_type;
      let discovered = self
        .scheduler
        .discovered_script(element, /* node_id */ 1, /* base_url_at_discovery */ None)?;
      let id = discovered.id;
      self.script_type_by_id.insert(id, script_type);
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

    fn module_graph_ready(&mut self, script_id: ScriptId, source_text: &str) -> Result<()> {
      let actions = self
        .scheduler
        .module_graph_ready(script_id, source_text.to_string())?;
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
  fn non_parser_external_scripts_are_async_by_default_and_do_not_block_parsing() -> Result<()> {
    let mut h = Harness::new();

    let script_id =
      h.discover_dynamic(classic_external_dynamic("dyn.js", false, false, /* force_async */ true))?;
    assert_eq!(h.started_fetches.len(), 1);
    assert!(
      h.blocked_parser_on.is_none(),
      "dynamic scripts must not block parsing"
    );

    h.fetch_complete(script_id, "DYN")?;
    // Completion should queue as a task.
    assert!(
      h.host.log.is_empty(),
      "async external scripts should not execute synchronously on fetch completion"
    );
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["script:DYN".to_string(), "microtask:DYN".to_string()]
    );
    Ok(())
  }

  #[test]
  fn in_order_asap_dynamic_external_scripts_execute_in_insertion_order_without_parsing_completed(
  ) -> Result<()> {
    let mut h = Harness::new();

    let a = h.discover_dynamic(classic_external_dynamic("a.js", false, false, /* force_async */ false))?;
    let b = h.discover_dynamic(classic_external_dynamic("b.js", false, false, /* force_async */ false))?;

    assert!(h.blocked_parser_on.is_none(), "dynamic scripts must not block parsing");
    assert_eq!(
      h.started_fetches
        .iter()
        .map(|(_id, _node_id, url, _destination, _credentials_mode)| url.as_str())
        .collect::<Vec<_>>(),
      vec!["a.js", "b.js"]
    );

    // Complete out-of-order: B finishes before A.
    h.fetch_complete(b, "B")?;
    h.run_event_loop()?;
    assert_eq!(h.host.log, Vec::<String>::new());

    h.fetch_complete(a, "A")?;
    h.run_event_loop()?;

    assert_eq!(
      h.host.log,
      vec![
        "script:A".to_string(),
        "microtask:A".to_string(),
        "script:B".to_string(),
        "microtask:B".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn async_dynamic_external_scripts_can_execute_before_in_order_asap_scripts() -> Result<()> {
    let mut h = Harness::new();

    let a = h.discover_dynamic(classic_external_dynamic("a.js", false, false, /* force_async */ false))?;
    let b = h.discover_dynamic(classic_external_dynamic("b.js", true, false, /* force_async */ false))?;

    // B is async-like and can run before A if it completes first.
    h.fetch_complete(b, "B")?;
    h.fetch_complete(a, "A")?;
    h.run_event_loop()?;

    assert_eq!(
      h.host.log,
      vec![
        "script:B".to_string(),
        "microtask:B".to_string(),
        "script:A".to_string(),
        "microtask:A".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn force_async_true_dynamic_external_scripts_behave_like_async() -> Result<()> {
    let mut h = Harness::new();

    let a = h.discover_dynamic(classic_external_dynamic("a.js", false, false, /* force_async */ true))?;
    let b = h.discover_dynamic(classic_external_dynamic("b.js", false, false, /* force_async */ true))?;

    // Async scripts execute in completion order.
    h.fetch_complete(b, "B")?;
    h.fetch_complete(a, "A")?;
    h.run_event_loop()?;

    assert_eq!(
      h.host.log,
      vec![
        "script:B".to_string(),
        "microtask:B".to_string(),
        "script:A".to_string(),
        "microtask:A".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn force_async_true_dynamic_module_external_scripts_behave_like_async() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let a = h.discover_dynamic(module_external_dynamic("a.js", /* async_attr */ false, /* force_async */ true))?;
    let b = h.discover_dynamic(module_external_dynamic("b.js", /* async_attr */ false, /* force_async */ true))?;

    assert!(
      h.blocked_parser_on.is_none(),
      "dynamic module scripts must not block parsing"
    );
    assert_eq!(
      h.started_module_graph_fetches,
      vec![
        (
          a,
          1u32,
          Some("a.js".to_string()),
          FetchDestination::ScriptCors,
          FetchCredentialsMode::SameOrigin,
        ),
        (
          b,
          1u32,
          Some("b.js".to_string()),
          FetchDestination::ScriptCors,
          FetchCredentialsMode::SameOrigin,
        ),
      ],
      "module scripts should request a module graph fetch"
    );

    // Async-like module scripts execute in completion order.
    h.module_graph_ready(b, "B")?;
    h.module_graph_ready(a, "A")?;
    h.run_event_loop()?;

    assert_eq!(
      h.host.log,
      vec![
        "script:B".to_string(),
        "microtask:B".to_string(),
        "script:A".to_string(),
        "microtask:A".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn in_order_asap_dynamic_inline_module_scripts_execute_in_insertion_order() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let a = h.discover_dynamic(module_inline_dynamic("A", /* async_attr */ false, /* force_async */ false))?;
    let b = h.discover_dynamic(module_inline_dynamic("B", /* async_attr */ false, /* force_async */ false))?;

    // Mark B ready first; it must not execute until A is ready.
    h.module_graph_ready(b, "B")?;
    h.run_event_loop()?;
    assert!(h.host.log.is_empty());

    // Now A becomes ready; both should execute in insertion order.
    h.module_graph_ready(a, "A")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec![
        "script:A".to_string(),
        "microtask:A".to_string(),
        "script:B".to_string(),
        "microtask:B".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn parser_inserted_inline_module_scripts_are_deferred_by_default_until_parsing_completed() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let module_id = h.discover(module_inline("M"))?;
    h.module_graph_ready(module_id, "M")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      Vec::<String>::new(),
      "module scripts must not execute synchronously during parsing"
    );

    h.parsing_completed()?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["script:M".to_string(), "microtask:M".to_string()]
    );
    Ok(())
  }

  #[test]
  fn non_parser_defer_attribute_is_ignored_and_still_runs_async() -> Result<()> {
    let mut h = Harness::new();

    let script_id =
      h.discover_dynamic(classic_external_dynamic("dyn.js", false, true, /* force_async */ true))?;
    assert_eq!(h.started_fetches.len(), 1);
    assert!(
      h.blocked_parser_on.is_none(),
      "dynamic scripts must not block parsing"
    );

    h.fetch_complete(script_id, "DYN")?;
    // Defer must not wait for parsing_completed for dynamic scripts.
    h.parsing_completed()?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["script:DYN".to_string(), "microtask:DYN".to_string()]
    );
    Ok(())
  }

  #[test]
  fn non_parser_inline_scripts_execute_as_task() -> Result<()> {
    let mut h = Harness::new();

    h.discover_dynamic(classic_inline_dynamic("x"))?;
    assert_eq!(
      h.host.log,
      vec!["script:x".to_string(), "microtask:x".to_string()]
    );
    Ok(())
  }

  #[test]
  fn src_attribute_present_but_invalid_does_not_execute_inline_or_start_fetch() -> Result<()> {
    let mut h = Harness::new();

    h.discover(ScriptElementSpec {
      base_url: None,
      // `src` attribute present, but no fetchable URL (e.g. empty string or a rejected scheme).
      src: None,
      src_attr_present: true,
      inline_text: "INLINE".to_string(),
      async_attr: false,
      defer_attr: false,
      nomodule_attr: false,
      force_async: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: true,
      node_id: None,
      script_type: ScriptType::Classic,
    })?;

    assert!(
      h.started_fetches.is_empty(),
      "expected no fetch to be started for invalid src"
    );
    assert!(
      h.host.log.is_empty(),
      "expected no inline execution when src attribute is present"
    );
    assert!(
      h.blocked_parser_on.is_none(),
      "invalid external scripts must not block parsing"
    );
    Ok(())
  }

  #[test]
  fn nomodule_inline_script_is_skipped_when_module_scripts_supported() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let mut spec = classic_inline("SKIP");
    spec.nomodule_attr = true;
    h.discover(spec)?;
    assert_eq!(h.host.log, Vec::<String>::new());

    h.discover(classic_inline("RUN"))?;
    assert_eq!(
      h.host.log,
      vec!["script:RUN".to_string(), "microtask:RUN".to_string()]
    );
    Ok(())
  }

  #[test]
  fn nomodule_external_script_does_not_start_fetch_when_module_scripts_supported() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let mut spec = classic_external("https://example.com/a.js", false, false);
    spec.nomodule_attr = true;
    h.discover(spec)?;

    assert!(
      h.started_fetches.is_empty(),
      "expected no fetch to be started for nomodule external scripts"
    );
    assert!(
      h.blocked_parser_on.is_none(),
      "nomodule external scripts must not block parsing"
    );
    assert!(h.host.log.is_empty(), "nomodule scripts must not execute");
    Ok(())
  }

  #[test]
  fn nomodule_applies_to_dynamic_inserted_scripts_too_when_module_scripts_supported() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    h.discover_dynamic(with_nomodule(classic_external_dynamic(
      "dyn.js", false, false, /* force_async */ true,
    )))?;
    h.discover_dynamic(with_nomodule(classic_inline_dynamic("INLINE")))?;

    assert!(
      h.host.log.is_empty(),
      "nomodule dynamic scripts must not execute"
    );
    assert!(
      h.started_fetches.is_empty(),
      "nomodule dynamic scripts must not start any fetch"
    );
    Ok(())
  }

  #[test]
  fn nomodule_does_not_apply_to_module_scripts() {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;

    let mut module = classic_inline("MODULE");
    module.script_type = ScriptType::Module;
    module.nomodule_attr = true;
    assert!(
      !module.is_suppressed_by_nomodule(&options),
      "nomodule must not block module scripts"
    );
  }

  #[test]
  fn module_scripts_are_ignored_when_module_scripts_not_supported() -> Result<()> {
    let mut h = Harness::new();

    h.discover(module_external("https://example.com/a.js", false))?;
    assert!(
      h.started_module_graph_fetches.is_empty(),
      "expected module scripts to be ignored when supports_module_scripts is false"
    );
    assert!(h.blocked_parser_on.is_none());
    assert!(h.host.log.is_empty());
    Ok(())
  }

  #[test]
  fn module_scripts_with_empty_src_queue_error_even_when_module_scripts_not_supported() -> Result<()> {
    let mut h = Harness::new();

    h.discover(module_external("", false))?;
    assert!(
      h.started_module_graph_fetches.is_empty(),
      "expected empty-src module scripts to not start module graph fetch"
    );
    h.run_event_loop()?;
    assert_eq!(h.host.log, vec!["event:error".to_string()]);
    Ok(())
  }

  #[test]
  fn module_scripts_with_invalid_src_do_not_execute_inline_fallback_even_when_module_scripts_not_supported(
  ) -> Result<()> {
    let mut h = Harness::new();

    h.discover(ScriptElementSpec {
      base_url: None,
      // `src` attribute present, but no fetchable URL (e.g. empty string or a rejected scheme).
      src: None,
      src_attr_present: true,
      inline_text: "INLINE".to_string(),
      async_attr: false,
      defer_attr: false,
      nomodule_attr: false,
      force_async: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      parser_inserted: true,
      node_id: None,
      script_type: ScriptType::Module,
    })?;
    assert!(
      h.started_module_graph_fetches.is_empty(),
      "expected invalid-src module scripts to not start module graph fetch"
    );
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["event:error".to_string()],
      "expected module scripts to queue an error event task and suppress inline fallback when src is invalid"
    );
    Ok(())
  }

  #[test]
  fn module_scripts_with_empty_src_queue_error_and_do_not_break_defer_queue() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    h.discover(module_external("", false))?;
    assert!(
      h.started_module_graph_fetches.is_empty(),
      "expected empty-src module scripts to not start module graph fetch"
    );
    h.run_event_loop()?;
    assert_eq!(h.host.log, vec!["event:error".to_string()]);

    let script_id = h.discover(module_external("https://example.com/a.js", false))?;
    assert_eq!(h.started_module_graph_fetches.len(), 1);
    h.module_graph_ready(script_id, "A")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["event:error".to_string()],
      "deferred module scripts must not execute before parsing_completed"
    );

    h.parsing_completed()?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec![
        "event:error".to_string(),
        "script:A".to_string(),
        "microtask:A".to_string(),
      ]
    );
    Ok(())
  }

  #[test]
  fn parser_inserted_module_scripts_are_deferred_by_default_and_use_scriptcors() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let script_id = h.discover(module_external("https://example.com/a.js", false))?;
    assert_eq!(
      h.started_module_graph_fetches,
      vec![(
        script_id,
        1u32,
        Some("https://example.com/a.js".to_string()),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::SameOrigin,
      )]
    );
    assert!(
      h.blocked_parser_on.is_none(),
      "module scripts must not block parsing"
    );

    h.module_graph_ready(script_id, "A")?;
    h.run_event_loop()?;
    assert!(
      h.host.log.is_empty(),
      "deferred module scripts must not execute before parsing_completed"
    );

    h.parsing_completed()?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["script:A".to_string(), "microtask:A".to_string()]
    );
    Ok(())
  }

  #[test]
  fn async_module_scripts_execute_asap_without_waiting_for_parsing_completed() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let script_id = h.discover(module_external("https://example.com/a.js", true))?;
    assert_eq!(h.started_module_graph_fetches.len(), 1);
    assert!(
      h.started_module_graph_fetches[0].3 == FetchDestination::ScriptCors,
      "module scripts must fetch with ScriptCors"
    );
    assert!(
      h.blocked_parser_on.is_none(),
      "module scripts must not block parsing"
    );

    h.module_graph_ready(script_id, "A")?;
    assert!(
      h.host.log.is_empty(),
      "async module scripts should not execute synchronously on fetch completion"
    );
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["script:A".to_string(), "microtask:A".to_string()]
    );
    Ok(())
  }

  #[test]
  fn module_external_script_fetches_with_cors_and_same_origin_credentials_by_default() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let mut spec = classic_external("https://example.com/a.js", false, false);
    spec.script_type = ScriptType::Module;
    let script_id = h.discover(spec)?;

    assert_eq!(
      h.started_module_graph_fetches,
      vec![(
        script_id,
        1u32,
        Some("https://example.com/a.js".to_string()),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::SameOrigin,
      )]
    );
    assert!(
      h.blocked_parser_on.is_none(),
      "module scripts must not block parsing"
    );
    Ok(())
  }

  #[test]
  fn module_crossorigin_use_credentials_sets_credentials_mode_include() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let mut spec = classic_external("https://example.com/a.js", false, false);
    spec.script_type = ScriptType::Module;
    spec.crossorigin = Some(crate::resource::CorsMode::UseCredentials);
    let script_id = h.discover(spec)?;

    assert_eq!(
      h.started_module_graph_fetches,
      vec![(
        script_id,
        1u32,
        Some("https://example.com/a.js".to_string()),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::Include,
      )]
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
      vec![(
        blocking_id,
        1u32,
        "https://example.com/a.js".to_string(),
        FetchDestination::Script,
        FetchCredentialsMode::Include,
      )]
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
  fn crossorigin_use_credentials_sets_credentials_mode_include() -> Result<()> {
    let mut h = Harness::new();

    let mut spec = classic_external("https://example.com/a.js", false, false);
    spec.crossorigin = Some(crate::resource::CorsMode::UseCredentials);
    let script_id = h.discover(spec)?;

    assert_eq!(
      h.started_fetches,
      vec![(
        script_id,
        1u32,
        "https://example.com/a.js".to_string(),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::Include,
      )]
    );
    Ok(())
  }

  #[test]
  fn dynamic_module_scripts_start_fetch_in_cors_mode_with_default_same_origin_credentials() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let script_id = h.discover_dynamic(module_external_dynamic(
      "https://example.com/a.js",
      /* async_attr */ false,
      /* force_async */ false,
    ))?;

    assert_eq!(
      h.started_module_graph_fetches,
      vec![(
        script_id,
        1u32,
        Some("https://example.com/a.js".to_string()),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::SameOrigin,
      )]
    );
    assert!(
      h.blocked_parser_on.is_none(),
      "dynamic module scripts must not block parsing"
    );
    Ok(())
  }

  #[test]
  fn dynamic_async_module_script_crossorigin_use_credentials_sets_credentials_mode_include() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let mut spec = module_external_dynamic(
      "https://example.com/a.js",
      /* async_attr */ false,
      /* force_async */ true,
    );
    spec.crossorigin = Some(crate::resource::CorsMode::UseCredentials);
    let script_id = h.discover_dynamic(spec)?;

    assert_eq!(
      h.started_module_graph_fetches,
      vec![(
        script_id,
        1u32,
        Some("https://example.com/a.js".to_string()),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::Include,
      )]
    );
    assert!(
      h.blocked_parser_on.is_none(),
      "dynamic module scripts must not block parsing"
    );
    Ok(())
  }

  #[test]
  fn importmaps_are_ignored_when_module_scripts_not_supported() -> Result<()> {
    // Browsers without module support treat `type="importmap"` like an unknown script type.
    let mut h = Harness::new();
    h.discover(importmap_inline("MAP"))?;
    h.run_event_loop()?;
    assert!(
      h.host.log.is_empty(),
      "import maps must be ignored when module scripts are not supported"
    );
    Ok(())
  }

  #[test]
  fn parser_inserted_importmap_executes_synchronously() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);
    h.discover(importmap_inline("MAP"))?;
    assert_eq!(
      h.host.log,
      vec!["script:MAP".to_string(), "microtask:MAP".to_string()]
    );
    Ok(())
  }

  #[test]
  fn dynamic_importmap_executes_as_task() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);
    h.discover_dynamic(importmap_inline_dynamic("MAP"))?;
    assert!(
      h.host.log.is_empty(),
      "dynamic import maps must not execute synchronously"
    );
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["script:MAP".to_string(), "microtask:MAP".to_string()]
    );
    Ok(())
  }

  #[test]
  fn importmap_scripts_with_src_queue_error_and_do_not_execute_inline_fallback() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let mut spec = classic_external("https://example.com/importmap.json", false, false);
    spec.script_type = ScriptType::ImportMap;
    spec.inline_text = "INLINE".to_string();
    h.discover(spec)?;

    assert!(
      h.started_fetches.is_empty(),
      "import map scripts with a src attribute must not start fetch"
    );
    assert!(
      h.started_module_graph_fetches.is_empty(),
      "import map scripts with a src attribute must not start module graph fetch"
    );
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["event:error".to_string()],
      "import map scripts with a src attribute must not execute their inline contents"
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

  #[test]
  fn inline_module_scripts_are_queued_as_tasks_not_executed_synchronously() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let module_id = h.discover_dynamic(module_inline_dynamic(
      "INLINE_MOD",
      /* async_attr */ false,
      /* force_async */ false,
    ))?;
    assert_eq!(
      h.started_module_graph_fetches,
      vec![(
        module_id,
        1u32,
        None,
        FetchDestination::ScriptCors,
        FetchCredentialsMode::SameOrigin,
      )],
      "expected scheduler to request module graph fetch for inline module"
    );
    assert!(
      h.host.log.is_empty(),
      "inline module scripts must not execute synchronously on discovery"
    );

    h.module_graph_ready(module_id, "INLINE_MOD")?;
    assert!(
      h.host.log.is_empty(),
      "inline module scripts should execute from a queued task, not during module_graph_ready"
    );

    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec![
        "script:INLINE_MOD".to_string(),
        "microtask:INLINE_MOD".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn parser_inserted_module_without_async_executes_after_parsing_completed() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let module_id = h.discover(module_external("m.js", false))?;
    assert_eq!(
      h.started_module_graph_fetches,
      vec![(
        module_id,
        1u32,
        Some("m.js".to_string()),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::SameOrigin,
      )]
    );

    // The module graph becomes ready before parsing is complete.
    h.module_graph_ready(module_id, "MOD")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      Vec::<String>::new(),
      "parser-inserted non-async module scripts must not execute before parsing_completed"
    );

    h.parsing_completed()?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec!["script:MOD".to_string(), "microtask:MOD".to_string()]
    );
    Ok(())
  }

  #[test]
  fn parser_inserted_async_module_can_execute_before_parsing_completes() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let module_id = h.discover(module_external("a.js", true))?;
    assert_eq!(
      h.started_module_graph_fetches,
      vec![(
        module_id,
        1u32,
        Some("a.js".to_string()),
        FetchDestination::ScriptCors,
        FetchCredentialsMode::SameOrigin,
      )]
    );

    h.module_graph_ready(module_id, "ASYNC_MOD")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec![
        "script:ASYNC_MOD".to_string(),
        "microtask:ASYNC_MOD".to_string()
      ]
    );
    Ok(())
  }

  #[test]
  fn dynamic_non_async_module_scripts_execute_in_insertion_order() -> Result<()> {
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut h = Harness::new_with_options(options);

    let m1 = h.discover_dynamic(module_external_dynamic("m1.js", false, /* force_async */ false))?;
    let m2 = h.discover_dynamic(module_external_dynamic("m2.js", false, /* force_async */ false))?;
    assert_eq!(
      h.started_module_graph_fetches,
      vec![
        (
          m1,
          1u32,
          Some("m1.js".to_string()),
          FetchDestination::ScriptCors,
          FetchCredentialsMode::SameOrigin,
        ),
        (
          m2,
          1u32,
          Some("m2.js".to_string()),
          FetchDestination::ScriptCors,
          FetchCredentialsMode::SameOrigin,
        ),
      ]
    );

    // Second module becomes ready first; it must not execute until the first module is ready.
    h.module_graph_ready(m2, "M2")?;
    h.run_event_loop()?;
    assert_eq!(h.host.log, Vec::<String>::new());

    // Now the first module is ready; both should queue in insertion order.
    h.module_graph_ready(m1, "M1")?;
    h.run_event_loop()?;
    assert_eq!(
      h.host.log,
      vec![
        "script:M1".to_string(),
        "microtask:M1".to_string(),
        "script:M2".to_string(),
        "microtask:M2".to_string(),
      ]
    );
    Ok(())
  }
}
