//! JavaScript engine embedding abstraction for the WPT DOM runner.
//!
//! `js-wpt-dom-runner` executes a curated subset of WPT `testharness.js` DOM tests. The runner
//! itself owns the harness logic (script ordering, META parsing, HTML script extraction, etc.).
//! This module defines the minimal interface an embedded JS engine must provide so we can swap
//! implementations (`ecma-rs/vm-js` today, QuickJS legacy backend) with minimal churn.
//!
//! The interface is intentionally **spec-shaped**:
//! - realm/global setup (`window`/`document`/`location`/timers/report hook)
//! - classic script evaluation (with a source name for stack traces)
//! - draining microtasks (Promise jobs)
//! - running "tasks"/timers (an event-loop tick)
//! - collecting the FastRender testharness report payload

use crate::wpt_report::WptReport;
use crate::RunError;
use std::time::Duration;

#[cfg(feature = "quickjs")]
pub mod quickjs;
#[cfg(feature = "vmjs")]
pub mod vmjs;

/// Hooks for integrating a JS realm with FastRender's browser host environment.
///
/// Today the runner uses JS shims for `window`/`document` and the event loop. The long-term
/// direction is to wire `vm-js` to real FastRender host objects and an HTML event loop.
///
/// HTML Standard terminology to keep in mind:
/// - **Task queues**: most Web APIs (including timers) enqueue *tasks*.
/// - **Microtask queue**: Promises/`queueMicrotask` enqueue microtasks; after running a task the
///   UA performs a **microtask checkpoint**.
///
/// These hooks are placeholders for that integration.
#[allow(dead_code)]
pub trait HostEnvironment {
  /// Queue a task into the HTML event loop.
  fn queue_task(&mut self, _task: HostTask) {}

  /// Queue a microtask.
  fn queue_microtask(&mut self, _task: HostTask) {}

  /// Schedule a timer.
  fn set_timeout(&mut self, _delay: Duration, _task: HostTask) -> TimerId {
    TimerId(0)
  }

  /// Cancel a scheduled timer.
  fn clear_timeout(&mut self, _id: TimerId) {}
}

/// Opaque timer handle returned by [`HostEnvironment::set_timeout`].
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u64);

/// Placeholder task callback type.
#[allow(dead_code)]
pub type HostTask = Box<dyn FnOnce() + 'static>;

/// Parameters used to create a fresh realm suitable for executing one WPT test.
#[derive(Debug, Clone)]
pub struct BackendInit {
  pub test_url: String,
  pub timeout: Duration,
  pub max_tasks: usize,
  pub max_microtasks: usize,
}

/// Backend interface required by the WPT DOM runner.
///
/// Each backend is responsible for:
/// - creating a fresh JS realm/context
/// - installing globals (`window`/`document`/timers/report hook)
/// - evaluating scripts (with a source name for stack traces)
/// - draining microtasks (Promise job queue)
/// - polling/running timers and other event-loop tasks
/// - mapping runner timeouts to engine interrupts / virtual time budgets
pub trait Backend {
  fn init_realm(
    &mut self,
    init: BackendInit,
    host: Option<&mut dyn HostEnvironment>,
  ) -> Result<(), RunError>;

  fn eval_script(&mut self, source: &str, name: &str) -> Result<(), RunError>;

  fn drain_microtasks(&mut self) -> Result<(), RunError>;

  /// Run one "tick" of the backend's event loop integration.
  ///
  /// Returns `true` if any work was performed (timers fired, tasks ran).
  fn poll_event_loop(&mut self) -> Result<bool, RunError>;

  fn take_report(&mut self) -> Result<Option<WptReport>, RunError>;

  fn is_timed_out(&self) -> bool;

  fn idle_wait(&mut self);
}
