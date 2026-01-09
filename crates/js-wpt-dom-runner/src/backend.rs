use crate::RunError;
use crate::wpt_report::WptReport;
use std::env;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
  QuickJs,
  VmJs,
}

impl BackendKind {
  pub fn as_str(self) -> &'static str {
    match self {
      BackendKind::QuickJs => "quickjs",
      BackendKind::VmJs => "vmjs",
    }
  }

  pub fn is_available(self) -> bool {
    match self {
      BackendKind::QuickJs => true,
      BackendKind::VmJs => crate::backend_vmjs::is_available(),
    }
  }

  pub fn preferred() -> Self {
    if BackendKind::VmJs.is_available() {
      BackendKind::VmJs
    } else {
      BackendKind::QuickJs
    }
  }

  pub fn all_available() -> Vec<Self> {
    let mut out = vec![BackendKind::QuickJs];
    if BackendKind::VmJs.is_available() {
      out.push(BackendKind::VmJs);
    }
    out
  }
}

impl std::fmt::Display for BackendKind {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendSelection {
  /// Choose the best backend available in the current build (prefer vm-js).
  Auto,
  QuickJs,
  VmJs,
}

impl BackendSelection {
  pub fn resolve(self) -> BackendKind {
    match self {
      BackendSelection::Auto => BackendKind::preferred(),
      BackendSelection::QuickJs => BackendKind::QuickJs,
      BackendSelection::VmJs => BackendKind::VmJs,
    }
  }

  /// Reads `FASTERENDER_WPT_DOM_BACKEND` if set.
  ///
  /// Accepted values: `auto` | `vmjs` | `quickjs`.
  pub fn from_env() -> Result<Option<Self>, RunError> {
    let Ok(raw) = env::var("FASTERENDER_WPT_DOM_BACKEND") else {
      return Ok(None);
    };
    let value = raw.trim().to_ascii_lowercase();
    let selection = match value.as_str() {
      "" | "auto" => BackendSelection::Auto,
      "vmjs" => BackendSelection::VmJs,
      "quickjs" => BackendSelection::QuickJs,
      other => {
        return Err(RunError::Js(format!(
          "invalid FASTERENDER_WPT_DOM_BACKEND={other:?} (expected auto|vmjs|quickjs)"
        )))
      }
    };
    Ok(Some(selection))
  }
}

impl Default for BackendSelection {
  fn default() -> Self {
    BackendSelection::Auto
  }
}

pub type BackendReport = WptReport;

#[derive(Debug, Clone)]
pub struct BackendInit {
  pub test_url: String,
  pub timeout: Duration,
  pub max_tasks: usize,
  pub max_microtasks: usize,
}

/// Backend interface required by the WPT DOM runner.
///
/// This is intentionally spec-shaped rather than engine-specific: each backend is responsible for:
/// - creating a fresh JS realm/context
/// - installing globals (`window`/`document`/timers/report hook)
/// - evaluating scripts
/// - draining microtasks (Promise job queue)
/// - polling/running timers and other event-loop tasks
/// - mapping runner timeouts to engine interrupts / virtual time budgets
pub trait Backend {
  fn init_realm(&mut self, init: BackendInit) -> Result<(), RunError>;

  fn eval_script(&mut self, source: &str) -> Result<(), RunError>;

  fn drain_microtasks(&mut self) -> Result<(), RunError>;

  /// Run one "tick" of the backend's event loop integration.
  ///
  /// Returns `true` if any work was performed (timers fired, tasks ran).
  fn poll_event_loop(&mut self) -> Result<bool, RunError>;

  fn take_report(&mut self) -> Result<Option<BackendReport>, RunError>;

  fn is_timed_out(&self) -> bool;

  fn idle_wait(&mut self);
}
