use crate::api::{ConsoleMessageLevel, SharedRenderDiagnostics};
use crate::error::{RenderStage, Result};
use crate::render_control::StageGuard;

use super::event_loop::{EventLoop, RunLimits, RunUntilIdleOutcome, TaskSource};

/// Host state for a JavaScript-enabled browsing context.
///
/// This is intentionally minimal: it exists primarily to provide a stable place to hang diagnostics
/// wiring while the JS + DOM integration layers are still under construction.
#[derive(Default)]
pub struct BrowserTabHost {
  diagnostics: Option<SharedRenderDiagnostics>,
  /// Arbitrary host-owned debug lines used by tests and UI plumbing.
  pub debug_log: Vec<String>,
}

impl BrowserTabHost {
  pub fn new(diagnostics: Option<SharedRenderDiagnostics>) -> Self {
    Self {
      diagnostics,
      debug_log: Vec::new(),
    }
  }

  pub fn set_diagnostics(&mut self, diagnostics: Option<SharedRenderDiagnostics>) {
    self.diagnostics = diagnostics;
  }

  pub fn diagnostics(&self) -> Option<&SharedRenderDiagnostics> {
    self.diagnostics.as_ref()
  }

  pub fn record_js_exception(&self, message: impl Into<String>, stack: Option<String>) {
    if let Some(diag) = &self.diagnostics {
      diag.record_js_exception(message, stack);
    }
  }

  pub fn record_console_message(&self, level: ConsoleMessageLevel, message: impl Into<String>) {
    if let Some(diag) = &self.diagnostics {
      diag.record_console_message(level, message);
    }
  }
}

/// A minimal "tab" that owns an HTML-like event loop plus an optional diagnostics sink.
///
/// The event loop is parameterized over [`BrowserTabHost`] so callbacks can mutate host state while
/// remaining independent from the renderer's main pipeline types.
pub struct BrowserTab {
  host: BrowserTabHost,
  event_loop: EventLoop<BrowserTabHost>,
}

impl BrowserTab {
  pub fn new(diagnostics: Option<SharedRenderDiagnostics>) -> Self {
    Self {
      host: BrowserTabHost::new(diagnostics),
      event_loop: EventLoop::new(),
    }
  }

  pub fn host(&self) -> &BrowserTabHost {
    &self.host
  }

  pub fn host_mut(&mut self) -> &mut BrowserTabHost {
    &mut self.host
  }

  pub fn event_loop(&self) -> &EventLoop<BrowserTabHost> {
    &self.event_loop
  }

  pub fn event_loop_mut(&mut self) -> &mut EventLoop<BrowserTabHost> {
    &mut self.event_loop
  }

  /// Queue an event-loop task.
  pub fn queue_task<F>(&mut self, source: TaskSource, runnable: F) -> Result<()>
  where
    F: FnOnce(&mut BrowserTabHost, &mut EventLoop<BrowserTabHost>) -> Result<()> + 'static,
  {
    self.event_loop.queue_task(source, runnable)
  }

  /// Execute a JS "script" (caller-provided) and capture any thrown exception into diagnostics.
  ///
  /// This helper intentionally does *not* propagate errors to callers: in a browser, an uncaught
  /// exception is reported but does not abort further execution.
  pub fn execute_script<F>(&mut self, runnable: F)
  where
    F: FnOnce(&mut BrowserTabHost, &mut EventLoop<BrowserTabHost>) -> Result<()>,
  {
    let _stage_guard = StageGuard::install(Some(RenderStage::Script));
    if let Err(err) = runnable(&mut self.host, &mut self.event_loop) {
      self.host.record_js_exception(err.to_string(), None);
    }
  }

  /// Drive the event loop until idle while capturing task errors as JS exceptions.
  pub fn run_until_idle(&mut self, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    let _stage_guard = StageGuard::install(Some(RenderStage::Script));
    let diagnostics = self.host.diagnostics.clone();
    self.event_loop.run_until_idle_handling_errors(
      &mut self.host,
      limits,
      move |err| {
        if let Some(diag) = &diagnostics {
          diag.record_js_exception(err.to_string(), None);
        }
      },
    )
  }
}

