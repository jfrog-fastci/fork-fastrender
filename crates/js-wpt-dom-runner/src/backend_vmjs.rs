use crate::backend::{Backend, BackendInit, BackendReport};
use crate::RunError;

#[cfg(feature = "backend_vmjs")]
use std::time::Duration;

pub(crate) fn is_available() -> bool {
  // The vm-js backend depends on FastRender's in-tree JS embedding + DOM bindings. Feature-gating
  // allows the QuickJS backend to remain usable while that integration is still evolving.
  #[cfg(feature = "backend_vmjs")]
  {
    // TODO: Flip to `true` once the vm-js embedding can execute the WPT harness end-to-end.
    false
  }
  #[cfg(not(feature = "backend_vmjs"))]
  {
    false
  }
}

#[cfg(feature = "backend_vmjs")]
pub struct VmJsBackend {
  clock: std::sync::Arc<fastrender::js::VirtualClock>,
  _event_loop: fastrender::js::EventLoop<()>,
  started_at: Duration,
  timeout: Duration,
}

#[cfg(not(feature = "backend_vmjs"))]
pub struct VmJsBackend;

impl VmJsBackend {
  pub fn new() -> Self {
    #[cfg(feature = "backend_vmjs")]
    {
      let clock = std::sync::Arc::new(fastrender::js::VirtualClock::new());
      let event_loop = fastrender::js::EventLoop::<()>::with_clock(clock.clone());
      Self {
        clock,
        _event_loop: event_loop,
        started_at: Duration::from_secs(0),
        timeout: Duration::from_secs(0),
      }
    }
    #[cfg(not(feature = "backend_vmjs"))]
    {
      Self
    }
  }
}

impl Default for VmJsBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl Backend for VmJsBackend {
  fn init_realm(&mut self, _init: BackendInit) -> Result<(), RunError> {
    #[cfg(not(feature = "backend_vmjs"))]
    {
      return Err(RunError::Js(
        "vm-js backend is not enabled; rebuild with --features backend_vmjs".to_string(),
      ));
    }
    #[cfg(feature = "backend_vmjs")]
    {
      self.started_at = self.clock.now();
      self.timeout = _init.timeout;
      // Wire-up notes:
      // - This backend is expected to execute `testharness.js` using the real vm-js runtime + DOM
      //   bindings, and drive asynchronous work using FastRender's `EventLoop` with a `VirtualClock`
      //   for deterministic timer advancement.
      //
      // This integration is intentionally feature-gated while the embedding lands across multiple
      // crates.
      return Err(RunError::Js("vm-js backend is not implemented yet".to_string()));
    }
  }

  fn eval_script(&mut self, _source: &str) -> Result<(), RunError> {
    Err(RunError::Js("vm-js backend is unavailable".to_string()))
  }

  fn drain_microtasks(&mut self) -> Result<(), RunError> {
    Err(RunError::Js("vm-js backend is unavailable".to_string()))
  }

  fn poll_event_loop(&mut self) -> Result<bool, RunError> {
    Err(RunError::Js("vm-js backend is unavailable".to_string()))
  }

  fn take_report(&mut self) -> Result<Option<BackendReport>, RunError> {
    Err(RunError::Js("vm-js backend is unavailable".to_string()))
  }

  fn is_timed_out(&self) -> bool {
    #[cfg(feature = "backend_vmjs")]
    {
      let elapsed = self.clock.now().saturating_sub(self.started_at);
      elapsed >= self.timeout
    }
    #[cfg(not(feature = "backend_vmjs"))]
    {
      true
    }
  }

  fn idle_wait(&mut self) {
    #[cfg(feature = "backend_vmjs")]
    {
      // In the vm-js backend we use a virtual clock for deterministic timers; "sleeping" advances
      // the clock instead of blocking the OS thread.
      self.clock.advance(Duration::from_millis(1));
    }
  }
}
